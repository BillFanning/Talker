//! The runtime orchestrator (spec §10, §13, §97.1, §113).
//!
//! [`Listener`] owns the Channel registry and drives the [`ChannelState`]
//! lifecycle (§8/§9): it builds live transports from validated config (via
//! [`super::build`]), opens/binds them at Start (resource failures → `Faulted`,
//! §71/§8.2), wires the stream pipeline, and stops them on request. All channels
//! share one [`RuntimeEvent`] stream (§137).
//!
//! Commands are exposed as async methods (`start`/`stop`/`apply_pending`, …) —
//! this method API *is* the command surface (there is no separate command enum;
//! ADR-012). Raw recording is wired for
//! serial/UDP channels from `RecordingConfig`; a recording-enable failure
//! surfaces a warning without faulting the Channel (§55). Accepted TCP
//! **connection** channels run the same stream pipeline as their listener (§16.2);
//! per-connection recording and snapshots remain deferred (§59 filename
//! templates; the supervisor keeps no per-connection handle).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::schema::InterfaceConfig;
use crate::config::ChannelConfig;
use crate::core::{ChannelId, ChannelState, DisplayViewId, RuntimeEvent};
use crate::display::{DisplayView, RenderedOutput};
use crate::record::{
    start_display_recording, start_raw_recording, DisplayFileRecorder, FileRotationPolicy,
    RawFileRecorder, Recording, RotatingDisplayRecorder, RotatingRawRecorder,
};
use crate::transport::{
    DataTransportRunner, SerialControlCommand, SerialControlHooks, SerialControlLines,
    TransportNotice,
};

use super::build::{build_display_view, build_serial, build_tcp_listener, build_udp, BuildError};
use super::channel::{
    spawn_monitored_channel, DataRecorder, MatchSetup, MonitoredChannel, TRANSPORT_NOTICES,
};
use super::pipeline::{DisplayViewHandle, PipelineCapacities, RawRecordingSettings};
use super::snapshot::{ChannelSnapshot, ChannelStats, StreamDelta};
use super::tcp::{start_tcp_listener, TcpListenerHandle};

/// How many Display Views a Channel runs (§48): one per configured view, at
/// least one (the pipeline's default view).
fn view_count(config: &ChannelConfig) -> usize {
    config.display.views.len().max(1)
}

/// Bounded inbox for live serial control-line commands (§161). Tiny: commands are
/// occasional operator actions.
const SERIAL_CONTROL_COMMANDS: usize = 8;

/// A live Channel's running tasks. Held by the orchestrator so it can stop them.
enum ChannelHandle {
    Data(MonitoredChannel),
    TcpListener(TcpListenerHandle),
}

/// Live serial control-line handle held by the orchestrator (§161): the command
/// inbox to the running serial reader, and the shared cell the reader updates with
/// the current line state.
struct SerialControl {
    commands: mpsc::Sender<SerialControlCommand>,
    state: Arc<Mutex<SerialControlLines>>,
}

/// Auto-reconnect backoff state for one faulted Channel (§9.1, §162). Created when
/// `reconnect_tick` first sees the fault; cleared on a successful reconnect or a
/// manual stop.
#[derive(Clone)]
struct ReconnectState {
    /// Reconnect attempts made so far (each a Stop+Start cycle).
    attempts: u32,
    /// The current backoff delay (grows by `multiplier`, capped at `max_backoff`).
    backoff: Duration,
    /// When the next attempt is due.
    next_attempt_at: Instant,
    /// Whether `max_attempts` was exhausted (stop retrying, stay Faulted).
    gave_up: bool,
}

/// Registry entry: the configuration, any accepted-but-unapplied change (§13),
/// the lifecycle state, and the live handle — usually present while Running, though a
/// spontaneous fault can leave a handle attached (effective state Faulted) until a
/// stop/recovery consumes it.
struct ManagedChannel {
    config: ChannelConfig,
    pending: Option<ChannelConfig>,
    state: ChannelState,
    handle: Option<ChannelHandle>,
    /// Pause handles for the running Channel's Display Views (§11, §48); empty
    /// while Stopped.
    display_handles: Vec<DisplayViewHandle>,
    /// Shared fault flag (ADR-006). The detached fault monitor flips it when the
    /// transport ends on a spontaneous fault; the orchestrator can't be mutated
    /// from that task, so it reads the flag to keep `state()` and command
    /// validation honest. A fresh flag is installed at each `start`.
    faulted: Arc<AtomicBool>,
    /// Live serial control-line handle (§161); `Some` only while a serial Channel
    /// is running.
    serial_control: Option<SerialControl>,
    /// Auto-reconnect backoff state (§9.1, §162); `Some` while a reconnect is
    /// pending for a faulted Channel with reconnect enabled.
    reconnect_state: Option<ReconnectState>,
}

impl ManagedChannel {
    /// The state a caller should see: a tripped fault flag overrides the stored
    /// lifecycle state, so a spontaneous transport fault reads as `Faulted`
    /// without waiting for a command to reconcile it (ADR-006).
    fn effective_state(&self) -> ChannelState {
        if self.faulted.load(Ordering::Relaxed) {
            ChannelState::Faulted
        } else {
            self.state
        }
    }
}

/// Errors from orchestrating a Channel.
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("unknown channel {0}")]
    UnknownChannel(ChannelId),
    #[error("unknown display view {0:?} on the channel")]
    UnknownDisplayView(DisplayViewId),
    #[error("illegal channel state transition from {from:?} to {to:?}")]
    IllegalTransition {
        from: ChannelState,
        to: ChannelState,
    },
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error("failed to open serial port: {0}")]
    SerialOpen(#[source] serialport::Error),
    #[error("failed to bind interface: {0}")]
    Bind(#[source] std::io::Error),
    #[error("serial control is not available for channel {0} (not a running serial channel)")]
    SerialControlUnavailable(ChannelId),
}

/// The runtime orchestrator (§97.1). Owns channels; drives their lifecycle.
pub struct Listener {
    channels: HashMap<ChannelId, ManagedChannel>,
    events_tx: mpsc::Sender<RuntimeEvent>,
    events_rx: Option<mpsc::Receiver<RuntimeEvent>>,
    caps: PipelineCapacities,
}

impl Listener {
    pub fn new(caps: PipelineCapacities) -> Self {
        let (events_tx, events_rx) = mpsc::channel(caps.events);
        Self {
            channels: HashMap::new(),
            events_tx,
            events_rx: Some(events_rx),
            caps,
        }
    }

    pub fn with_default_capacities() -> Self {
        Self::new(PipelineCapacities::default())
    }

    /// Take the shared runtime→UI event stream (§137). Available once.
    pub fn take_events(&mut self) -> Option<mpsc::Receiver<RuntimeEvent>> {
        self.events_rx.take()
    }

    /// Register a configured Channel (Stopped). The runtime mints its
    /// `ChannelId` (§97.1); the persisted `StableConfigId` in the config is a
    /// separate identity.
    pub fn add_channel(&mut self, config: ChannelConfig) -> ChannelId {
        let id = ChannelId::new();
        self.channels.insert(
            id,
            ManagedChannel {
                config,
                pending: None,
                state: ChannelState::Stopped,
                handle: None,
                display_handles: Vec::new(),
                faulted: Arc::new(AtomicBool::new(false)),
                serial_control: None,
                reconnect_state: None,
            },
        );
        id
    }

    pub fn state(&self, id: ChannelId) -> Option<ChannelState> {
        self.channels.get(&id).map(|c| c.effective_state())
    }

    pub fn config(&self, id: ChannelId) -> Option<&ChannelConfig> {
        self.channels.get(&id).map(|c| &c.config)
    }

    /// Update a Channel's stored per-channel **view** config in place, without a
    /// restart (§78, §87): the display config and the scroll-buffer `retention`. The
    /// viewer's presentation (mode, font, colors) is rendered GUI-side and the GUI caps
    /// its own scrollback live, so neither affects the live transport/pipeline — this
    /// only keeps the stored config current so a profile save captures the settings and
    /// the runtime's retention adopts the new limit on the Channel's next start. Unknown
    /// id is ignored.
    pub fn set_view_config(
        &mut self,
        id: ChannelId,
        display: crate::config::DisplayConfig,
        retention: crate::config::RetentionConfig,
    ) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.config.display = display;
            channel.config.retention = retention;
        }
    }

    /// Update a Channel's stored **Raw recording** config in place, without a restart
    /// (ADR-012/-013). Raw recording is a live field — the running recorder is (re)armed
    /// separately by [`set_recording`](Self::set_recording); this only keeps the stored
    /// config current so a profile save captures the destination/rotation/"record on
    /// start" settings. Unknown id is ignored.
    pub fn set_raw_recording_config(
        &mut self,
        id: ChannelId,
        raw: crate::config::RawRecordingConfig,
    ) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.config.raw_recording = raw;
        }
    }

    /// Request an on-demand snapshot of a running Channel's *small* observable state
    /// (§137, ADR-006): diagnostics, recent match firings, per-view pause state,
    /// recording state, liveness, and the stream end offset. The scrollback bytes
    /// come separately via [`stream_delta`](Self::stream_delta). Returns `None` when
    /// the Channel is unknown, not running, or a TCP listener (its connections are
    /// snapshot targets in their own right; per-connection snapshots are deferred).
    /// The `RuntimeEvent` stream stays the authoritative liveness signal.
    pub async fn snapshot(&self, id: ChannelId) -> Option<ChannelSnapshot> {
        match self.channels.get(&id)?.handle.as_ref()? {
            ChannelHandle::Data(tasks) => tasks.snapshot().await,
            ChannelHandle::TcpListener(_) => None,
        }
    }

    /// Cheap O(1) liveness stats for a running data Channel (§91.1, ADR-006) — the
    /// counters a multi-channel overview shows per tab, without cloning the
    /// scrollback. `None` when unknown, not running, or a TCP listener.
    pub async fn channel_stats(&self, id: ChannelId) -> Option<ChannelStats> {
        match self.channels.get(&id)?.handle.as_ref()? {
            ChannelHandle::Data(tasks) => tasks.stats().await,
            ChannelHandle::TcpListener(_) => None,
        }
    }

    /// Incremental stream bytes since the consumer's cursor (§87, ADR-009): only
    /// what is new, so a live viewer never re-ships the whole ~1 MB scrollback each
    /// poll. `None` when unknown, not running, or a TCP listener.
    pub async fn stream_delta(&self, id: ChannelId, since: u64) -> Option<StreamDelta> {
        match self.channels.get(&id)?.handle.as_ref()? {
            ChannelHandle::Data(tasks) => tasks.stream_delta(since).await,
            ChannelHandle::TcpListener(_) => None,
        }
    }

    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.keys().copied().collect()
    }

    /// Begin or stop Raw recording on a running data Channel live, without a restart
    /// (§50.2, ADR-012) — the manual counterpart of the match-rule `Record` action,
    /// sharing the pipeline's lazy begin / clean finalize path. `enabled = true`
    /// begins (a no-op if already recording, or if no destination is configured);
    /// `false` stops and finalizes. Returns `false` when the Channel is unknown, not
    /// running, or a TCP listener. The outcome is observed via the snapshot's
    /// recording state, and a begin failure raises a `WarningRaised` event (§55).
    pub async fn set_recording(
        &self,
        id: ChannelId,
        enabled: bool,
        raw: crate::config::RawRecordingConfig,
    ) -> bool {
        // Use the settings the caller read at click time (the editor's current
        // values), not committed config — so Record records to exactly what's on
        // screen, with no Apply/restart (ADR-012). The channel name comes from the
        // committed config (it's a live rename, not part of the recording settings).
        let Some(channel) = self.channels.get(&id) else {
            return false;
        };
        let settings = self.settings_from(&raw, channel.config.name.as_str());
        match channel.handle.as_ref() {
            Some(ChannelHandle::Data(tasks)) => tasks.set_recording(enabled, settings).await,
            _ => false,
        }
    }

    /// Drive the RTS output line of a running serial Channel (§161).
    pub async fn set_rts(&self, id: ChannelId, on: bool) -> Result<(), OrchestratorError> {
        self.serial_command(id, SerialControlCommand::SetRts(on))
            .await
    }

    /// Drive the DTR output line of a running serial Channel (§161).
    pub async fn set_dtr(&self, id: ChannelId, on: bool) -> Result<(), OrchestratorError> {
        self.serial_command(id, SerialControlCommand::SetDtr(on))
            .await
    }

    async fn serial_command(
        &self,
        id: ChannelId,
        command: SerialControlCommand,
    ) -> Result<(), OrchestratorError> {
        let control = self
            .channels
            .get(&id)
            .and_then(|c| c.serial_control.as_ref())
            .ok_or(OrchestratorError::SerialControlUnavailable(id))?;
        control
            .commands
            .send(command)
            .await
            .map_err(|_| OrchestratorError::SerialControlUnavailable(id))
    }

    /// The current serial control-line state of a running serial Channel (§161, the
    /// pull side of `ControlLinesChanged`); `None` if it is not a running serial
    /// Channel.
    pub fn serial_control_lines(&self, id: ChannelId) -> Option<SerialControlLines> {
        let control = self.channels.get(&id)?.serial_control.as_ref()?;
        control.state.lock().ok().map(|guard| *guard)
    }

    pub fn has_pending(&self, id: ChannelId) -> bool {
        self.channels.get(&id).is_some_and(|c| c.pending.is_some())
    }

    /// The accepted-but-unapplied configuration for a Channel (§13), if any.
    pub fn pending(&self, id: ChannelId) -> Option<&ChannelConfig> {
        self.channels.get(&id).and_then(|c| c.pending.as_ref())
    }

    /// The Display View ids of a running Channel (§48); empty while Stopped.
    pub fn display_views(&self, channel: ChannelId) -> Vec<DisplayViewId> {
        self.channels
            .get(&channel)
            .map(|c| c.display_handles.iter().map(|h| h.id).collect())
            .unwrap_or_default()
    }

    /// Pause one Display View (§11, §50): reception, recording, numbering, and
    /// other views are unaffected.
    pub fn pause_display(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<(), OrchestratorError> {
        self.display_handle(channel, view)?.pause();
        Ok(())
    }

    /// Resume a paused Display View (§11).
    pub fn resume_display(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<(), OrchestratorError> {
        self.display_handle(channel, view)?.resume();
        Ok(())
    }

    fn display_handle(
        &self,
        channel: ChannelId,
        view: DisplayViewId,
    ) -> Result<&DisplayViewHandle, OrchestratorError> {
        let managed = self
            .channels
            .get(&channel)
            .ok_or(OrchestratorError::UnknownChannel(channel))?;
        managed
            .display_handles
            .iter()
            .find(|h| h.id == view)
            .ok_or(OrchestratorError::UnknownDisplayView(view))
    }

    fn set_state(&mut self, id: ChannelId, state: ChannelState) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.state = state;
        }
    }

    /// Start a Channel (§10.1): Stopped → Starting → Running, opening the
    /// interface. A resource failure takes Starting → Faulted (§8.2, §71).
    pub async fn start(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let config = {
            let channel = self
                .channels
                .get(&id)
                .ok_or(OrchestratorError::UnknownChannel(id))?;
            let from = channel.effective_state();
            if !from.can_transition_to(ChannelState::Starting) {
                return Err(OrchestratorError::IllegalTransition {
                    from,
                    to: ChannelState::Starting,
                });
            }
            channel.config.clone()
        };

        // Install a fresh fault flag for this run (ADR-006); the monitor flips it
        // on a spontaneous fault and `state()`/validation read it back.
        let faulted = Arc::new(AtomicBool::new(false));
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.faulted = faulted.clone();
            channel.state = ChannelState::Starting;
        }
        match self.spawn_channel(id, &config, faulted).await {
            Ok((handle, serial_control)) => {
                let display_handles = match &handle {
                    ChannelHandle::Data(tasks) => tasks.display_handles().to_vec(),
                    // A TCP listener has no display itself; its connections do.
                    ChannelHandle::TcpListener(_) => Vec::new(),
                };
                if let Some(channel) = self.channels.get_mut(&id) {
                    channel.handle = Some(handle);
                    channel.display_handles = display_handles;
                    channel.serial_control = serial_control;
                    channel.state = ChannelState::Running;
                }
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelStarted(id));
                Ok(())
            }
            Err(err) => {
                self.set_state(id, ChannelState::Faulted);
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelFaulted(id));
                Err(err)
            }
        }
    }

    /// Stop a Channel (§10.2). A Running Channel is stopped gracefully (§110); a
    /// Faulted Channel is returned to Stopped (§8.5). Other states are illegal.
    ///
    /// Split into three phases so a caller can bound only the *await* (the graceful
    /// drain) without abandoning the bookkeeping: `begin_stop` marks Stopping and takes
    /// the handle out (cheap, synchronous), `drain_handle` awaits the handle's
    /// shutdown (the only part that can hang), and `finish_stop` lands the channel in
    /// Stopped and announces it. `shutdown` uses the phases directly so a timed-out
    /// drain still runs `finish_stop`; everyone else uses this convenience wrapper.
    pub async fn stop(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let handle = self.begin_stop(id)?;
        drain_handle(handle).await;
        self.finish_stop(id);
        Ok(())
    }

    /// Phase 1 of [`stop`](Self::stop): validate the transition, mark a Running
    /// channel Stopping, and take its handle out (so the slow drain in `drain_handle`
    /// owns no `&mut self`). Returns the handle to drain (or `None` if there was none,
    /// e.g. a start-time fault). Illegal from any state but Running/Faulted.
    fn begin_stop(&mut self, id: ChannelId) -> Result<Option<ChannelHandle>, OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        match state {
            // Running → graceful drain; Faulted → §8.5 recovery. Both consume the
            // handle (a spontaneous fault leaves one whose tasks have already ended; a
            // start-time fault leaves none) and land in Stopped.
            ChannelState::Running | ChannelState::Faulted => {
                if state == ChannelState::Running {
                    self.set_state(id, ChannelState::Stopping);
                }
                Ok(self.channels.get_mut(&id).and_then(|c| c.handle.take()))
            }
            other => Err(OrchestratorError::IllegalTransition {
                from: other,
                to: ChannelState::Stopped,
            }),
        }
    }

    /// Land a Channel in Stopped: clear its Display Views, reset the fault flag
    /// (so a previously-faulted Channel reads Stopped, not Faulted), and announce
    /// the stop (§110).
    fn finish_stop(&mut self, id: ChannelId) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.display_handles.clear();
            channel.serial_control = None;
            channel.reconnect_state = None;
            channel.state = ChannelState::Stopped;
            channel.faulted.store(false, Ordering::Relaxed);
        }
        let _ = self.events_tx.try_send(RuntimeEvent::ChannelStopped(id));
    }

    /// Accept a configuration change into pending state without applying it
    /// (§13). The active interface keeps its current settings until applied.
    pub fn set_pending_config(
        &mut self,
        id: ChannelId,
        config: ChannelConfig,
    ) -> Result<(), OrchestratorError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        channel.pending = Some(config);
        Ok(())
    }

    /// Apply a pending configuration via one coordinated restart (§13): stop,
    /// swap in the pending config, then start (only if it was Running).
    pub async fn apply_pending(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        if !self.channels.contains_key(&id) {
            return Err(OrchestratorError::UnknownChannel(id));
        }
        if !self.has_pending(id) {
            return Ok(());
        }
        let was_running = self.state(id) == Some(ChannelState::Running);
        if was_running {
            self.stop(id).await?;
        }
        if let Some(channel) = self.channels.get_mut(&id) {
            if let Some(pending) = channel.pending.take() {
                channel.config = pending;
            }
        }
        if was_running {
            self.start(id).await?;
        }
        Ok(())
    }

    /// Stop a Channel only if it is in a state `stop` accepts (Running or Faulted),
    /// returning whether a stop actually happened. A no-op (returns `false`) for any
    /// other state — chiefly Stopped — so callers that just want "make sure it's down"
    /// don't have to special-case the already-Stopped `IllegalTransition`. (Stopping is
    /// transient inside a single `stop` call and never observed across an await, so it
    /// isn't a reachable input here.) Unknown id still errors.
    pub async fn stop_if_live(&mut self, id: ChannelId) -> Result<bool, OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        if matches!(state, ChannelState::Running | ChannelState::Faulted) {
            self.stop(id).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// The one lifecycle primitive the command layer drives (§10, §13): optionally
    /// commit a new config, optionally (re)start, in one coordinated, server-side
    /// sequence — so single-channel and bulk flows share exactly one path and the
    /// Faulted→Stopped recovery lives here, not in the GUI.
    ///
    /// - `config: Some(c)` swaps `c` in as the active config (a live channel is
    ///   stopped first so it restarts onto `c`; a stopped one just adopts it).
    /// - `start = true` brings the channel up afterward. A Faulted channel is
    ///   normalized to Stopped first (§8.5), so the direct `Stopped → Starting`
    ///   transition is always legal — the illegal `Faulted → Starting` can't occur.
    ///
    /// With `config = None, start = true` this is a plain start/retry; with
    /// `config = Some, start = true` it is "Apply & Restart"/"Start with this config".
    /// With `config = Some, start = false` it commits the config and leaves the channel
    /// Stopped — note a *live* channel is stopped to adopt it (it does not stay up on
    /// the old config); use `start = true` to bring it back.
    pub async fn commit_and_start(
        &mut self,
        id: ChannelId,
        config: Option<ChannelConfig>,
        start: bool,
    ) -> Result<(), OrchestratorError> {
        if !self.channels.contains_key(&id) {
            return Err(OrchestratorError::UnknownChannel(id));
        }
        if let Some(config) = config {
            // Stop a live channel so it comes back up on the new config; then swap it in.
            self.stop_if_live(id).await?;
            if let Some(channel) = self.channels.get_mut(&id) {
                channel.pending = None; // the explicit config supersedes any queued one
                channel.config = config;
            }
        }
        if start {
            // Normalize Faulted/Reconnecting → Stopped so Start is a legal
            // Stopped→Starting transition (§8.5); a Running channel is already up.
            if self.state(id) != Some(ChannelState::Running) {
                self.stop_if_live(id).await?;
                self.start(id).await?;
            }
        }
        Ok(())
    }

    /// Rename a Channel in place (§6). The name is a user-facing label only and the
    /// pipeline does not key off it, so this takes effect immediately — no restart,
    /// no interface churn — unlike the full reconfigure path. A queued pending config
    /// is renamed too, so applying it later does not revert the name.
    pub fn rename(
        &mut self,
        id: ChannelId,
        name: crate::core::ChannelName,
    ) -> Result<(), OrchestratorError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        channel.config.name = name.clone();
        if let Some(pending) = &mut channel.pending {
            pending.name = name;
        }
        Ok(())
    }

    /// Remove a Channel from the registry entirely (e.g. a GUI "remove", or
    /// discarding a misconfigured channel). If it is Running or Faulted it is first
    /// stopped (best-effort) so its tasks and socket are released, then it is
    /// dropped. Unknown id is an error. After this, `state(id)` is `None`.
    pub async fn remove_channel(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        if matches!(state, ChannelState::Running | ChannelState::Faulted) {
            // Release the interface/tasks; ignore a stop error — we're discarding it.
            let _ = self.stop(id).await;
        }
        self.channels.remove(&id);
        Ok(())
    }

    /// Stop every live Channel (§113, application exit). Includes spontaneously
    /// faulted channels so their tasks and Display Views are cleaned up too.
    /// Per-channel grace window for [`shutdown`](Self::shutdown): generous, since a
    /// normal graceful stop drains the small bounded ingest queue and finalizes
    /// files in milliseconds — this only guards against a pathologically stuck
    /// finalize hanging process exit.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

    pub async fn shutdown(&mut self) {
        let live: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| {
                matches!(
                    c.effective_state(),
                    ChannelState::Running | ChannelState::Faulted
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in live {
            // Graceful stop (§110) preserves the accepted backlog — it drains the
            // bounded ingest queue into recordings before finalizing — so we prefer
            // it over a forced abort, which would abandon up to `caps.ingest`
            // buffered chunks. But it is *bounded*: should a recorder's finalize hang,
            // the timeout abandons the *wait* so process exit can't deadlock.
            //
            // Crucially we time out only the drain, not the bookkeeping: `begin_stop`
            // takes the handle out (cancelling the transport, so detached tasks wind
            // down on their own), and `finish_stop` always runs afterward — so even a
            // timed-out channel lands in Stopped, clears its handles, and emits
            // ChannelStopped, rather than being abandoned mid-`Stopping`.
            let Ok(handle) = self.begin_stop(id) else {
                continue; // not stoppable (shouldn't happen — we filtered to live)
            };
            let _ = tokio::time::timeout(Self::SHUTDOWN_GRACE, drain_handle(handle)).await;
            self.finish_stop(id);
        }
    }

    /// Drive auto-reconnect (§9.1, §162). The application calls this periodically;
    /// the orchestrator has no background loop (ADR-006). For each Channel that is
    /// effectively Faulted with reconnect enabled, the first call arms a backoff
    /// timer; once the backoff elapses, this performs a Stop+Start reconnect
    /// (emitting `ChannelReconnecting`/`ChannelReconnected`), backing off
    /// exponentially on failure and giving up (`ChannelReconnectGaveUp`) after
    /// `max_attempts`. Stop+Start reuses the tested lifecycle (Faulted → Stopped →
    /// Starting → Running), so the events also include the intermediate
    /// `ChannelStopped`/`ChannelStarted`.
    pub async fn reconnect_tick(&mut self) {
        let now = Instant::now();
        let candidates: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| {
                c.effective_state() == ChannelState::Faulted && c.config.reconnect.enabled
            })
            .map(|(id, _)| *id)
            .collect();

        for id in candidates {
            let Some(policy) = self.channels.get(&id).map(|c| c.config.reconnect) else {
                continue;
            };
            let state = self
                .channels
                .get(&id)
                .and_then(|c| c.reconnect_state.clone());
            match state {
                // First observation of the fault: arm the backoff timer.
                None => {
                    let backoff = Duration::from_millis(policy.initial_backoff_ms);
                    self.set_reconnect_state(
                        id,
                        ReconnectState {
                            attempts: 0,
                            backoff,
                            next_attempt_at: now + backoff,
                            gave_up: false,
                        },
                    );
                }
                // Given up, or not due yet.
                Some(s) if s.gave_up || now < s.next_attempt_at => {}
                Some(s) => {
                    if policy.max_attempts.is_some_and(|max| s.attempts >= max) {
                        let _ = self
                            .events_tx
                            .try_send(RuntimeEvent::ChannelReconnectGaveUp(id));
                        if let Some(rs) = self
                            .channels
                            .get_mut(&id)
                            .and_then(|c| c.reconnect_state.as_mut())
                        {
                            rs.gave_up = true;
                        }
                        continue;
                    }
                    let attempt = s.attempts + 1;
                    let _ = self
                        .events_tx
                        .try_send(RuntimeEvent::ChannelReconnecting(id, attempt));
                    // stop() clears reconnect_state; we re-establish it on failure.
                    let _ = self.stop(id).await;
                    if self.start(id).await.is_ok() {
                        let _ = self
                            .events_tx
                            .try_send(RuntimeEvent::ChannelReconnected(id));
                    } else {
                        let next_ms = (s.backoff.as_millis() as f64 * policy.multiplier) as u64;
                        let backoff =
                            Duration::from_millis(next_ms.min(policy.max_backoff_ms).max(1));
                        self.set_reconnect_state(
                            id,
                            ReconnectState {
                                attempts: attempt,
                                backoff,
                                next_attempt_at: Instant::now() + backoff,
                                gave_up: false,
                            },
                        );
                    }
                }
            }
        }
    }

    fn set_reconnect_state(&mut self, id: ChannelId, state: ReconnectState) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.reconnect_state = Some(state);
        }
    }

    /// Per-channel pipeline capacities, applying this channel's retention limits
    /// (§80, §88) on top of the base capacities.
    fn channel_caps(&self, config: &ChannelConfig) -> PipelineCapacities {
        let retention = &config.retention;
        PipelineCapacities {
            stream_display: retention.byte_limit.unwrap_or(self.caps.stream_display),
            event_retention: retention.event_limit,
            warning_retention: retention.warning_limit,
            error_retention: retention.error_limit,
            ..self.caps
        }
    }

    /// Build, open/bind, and wire a Channel's runtime tasks (§8.2). `faulted` is
    /// the run's shared fault flag (ADR-006), handed to the data-channel monitor.
    async fn spawn_channel(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
        faulted: Arc<AtomicBool>,
    ) -> Result<(ChannelHandle, Option<SerialControl>), OrchestratorError> {
        match &config.interface {
            InterfaceConfig::Serial(serial) => {
                // Serial is the one transport whose reader can stall (§97.1); give
                // it a notice sender so a sustained stall becomes a retained
                // diagnostic + warning in the pipeline (§101, ADR-007).
                let (notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                // Live control lines (§161): a command inbox to the reader and a
                // shared cell it updates; ControlLinesChanged flows on the events.
                let (cmd_tx, cmd_rx) = mpsc::channel(SERIAL_CONTROL_COMMANDS);
                let state = Arc::new(Mutex::new(SerialControlLines::default()));
                let hooks = SerialControlHooks {
                    commands: cmd_rx,
                    state: state.clone(),
                    events: self.events_tx.clone(),
                };
                let opened = build_serial(id, serial)?
                    .open()
                    .await
                    .map_err(OrchestratorError::SerialOpen)?
                    .with_notice_sender(notice_tx)
                    .with_control(hooks);
                let raw = self.build_raw_recorder(id, config).await;
                let display = self.build_display_recorder(id, config).await;
                let handle = ChannelHandle::Data(
                    self.spawn_data(id, opened, config, raw, display, faulted, notice_rx),
                );
                Ok((
                    handle,
                    Some(SerialControl {
                        commands: cmd_tx,
                        state,
                    }),
                ))
            }
            InterfaceConfig::Udp(udp) => {
                let bound = build_udp(id, udp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?;
                let raw = self.build_raw_recorder(id, config).await;
                let display = self.build_display_recorder(id, config).await;
                // UDP is async and never stalls the reader; no notices to send.
                let (_notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                let handle = ChannelHandle::Data(
                    self.spawn_data(id, bound, config, raw, display, faulted, notice_rx),
                );
                Ok((handle, None))
            }
            // The TCP listener supervises per-connection faults itself; a
            // listener-acceptor fault isn't reconciled through this flag (v1).
            InterfaceConfig::TcpListener(tcp) => {
                let bound = build_tcp_listener(id, tcp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?;
                // Per-connection recording is deferred (§59).
                let handle = start_tcp_listener(
                    bound,
                    self.channel_caps(config),
                    tcp.max_connections,
                    self.events_tx.clone(),
                );
                Ok((ChannelHandle::TcpListener(handle), None))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_data<R: DataTransportRunner>(
        &self,
        id: ChannelId,
        runner: R,
        config: &ChannelConfig,
        data_recorder: Option<DataRecorder>,
        display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
        faulted: Arc<AtomicBool>,
        notices_rx: mpsc::Receiver<TransportNotice>,
    ) -> MonitoredChannel {
        spawn_monitored_channel(
            id,
            runner,
            data_recorder,
            display_recorder,
            // One runtime Display View per configured view (§48); at least one.
            view_count(config),
            // Disk-space guard (§56.2, §168): only when both a guard and a Raw
            // recording destination are configured (the guard protects Raw, §168).
            config
                .raw_recording
                .disk_guard
                .zip(config.raw_recording.destination.clone()),
            // Match Rules (§50.2, §165) plus the Raw recording settings a match-
            // triggered `Record` needs (lazy-create from the destination — nothing
            // until a match fires).
            MatchSetup {
                rules: config.match_rules.clone(),
                recording_settings: self.recording_settings(config),
            },
            self.channel_caps(config),
            self.events_tx.clone(),
            faulted,
            notices_rx,
        )
    }

    /// The Raw recording settings for a `Record` action (§50.2): present whenever a
    /// Raw destination is configured — **independent of `raw_recording.enabled`**, so
    /// a match-triggered `Record` works even when auto-start recording is off. Mirrors
    /// `build_raw_recorder`'s destination/overwrite/timestamp/rotation choices so a
    /// match-triggered recording matches what auto-start recording would produce.
    fn recording_settings(&self, config: &ChannelConfig) -> Option<RawRecordingSettings> {
        self.settings_from(&config.raw_recording, config.name.as_str())
    }

    /// Build [`RawRecordingSettings`] from a Raw recording config + channel name — used
    /// by both the build-time path (`recording_settings`) and the live `set_recording`
    /// path, which passes the settings read from the editor at click time (ADR-012).
    /// `None` when no destination is set.
    fn settings_from(
        &self,
        raw: &crate::config::RawRecordingConfig,
        channel_name: &str,
    ) -> Option<RawRecordingSettings> {
        raw.destination
            .clone()
            .map(|destination| RawRecordingSettings {
                destination,
                channel_name: channel_name.to_string(),
                overwrite: raw.overwrite_policy,
                timestamps: raw.timestamp_enabled,
                file_rotation: raw.file_rotation,
                capacity: self.caps.raw_recording,
            })
    }

    /// Create the Raw Recording handle for a Channel if enabled (§53). Per §55,
    /// failing to open the file (e.g. a refused overwrite, §121) does **not**
    /// fault the Channel: recording stays disabled, a warning is surfaced, and
    /// reception continues (§5.8). Returns `None` when disabled or on failure.
    async fn build_raw_recorder(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
    ) -> Option<DataRecorder> {
        let recording = &config.raw_recording;
        if !recording.enabled {
            return None;
        }
        let Some(destination) = &recording.destination else {
            // Enabled but no destination — cannot record; surface a warning.
            let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
            return None;
        };
        let policy = recording.overwrite_policy;
        let ts = recording.timestamp_enabled;
        let cap = self.caps.raw_recording;
        // With rotation, `destination` is a directory and files are named per
        // period from the channel name (§59); otherwise it is a single file path.
        let created = if recording.file_rotation == FileRotationPolicy::None {
            RawFileRecorder::create(destination, policy, ts)
                .await
                .map(|r| start_raw_recording(r, cap))
        } else {
            RotatingRawRecorder::create(
                destination,
                config.name.as_str(),
                ".raw",
                policy,
                ts,
                recording.file_rotation,
            )
            .await
            .map(|r| start_raw_recording(r, cap))
        };
        match created {
            Ok(rec) => Some(DataRecorder::Raw(rec)),
            Err(_err) => {
                let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
                None
            }
        }
    }

    /// Create the Display Recording for a Channel if enabled (§54). v1 records
    /// the primary (first) Display View; an enable failure surfaces a warning
    /// without faulting the Channel (§55), like raw recording.
    async fn build_display_recorder(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
    ) -> Option<(DisplayView, Recording<RenderedOutput>)> {
        let recording = &config.display_recording;
        if !recording.enabled {
            return None;
        }
        let Some(destination) = &recording.destination else {
            let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
            return None;
        };
        let renderer = config
            .display
            .views
            .first()
            .map(build_display_view)
            .unwrap_or_default();
        let policy = recording.overwrite_policy;
        let ts = recording.timestamp_enabled;
        let cap = self.caps.raw_recording;
        let created = if recording.file_rotation == FileRotationPolicy::None {
            DisplayFileRecorder::create(destination, policy, ts)
                .await
                .map(|r| start_display_recording(r, cap))
        } else {
            RotatingDisplayRecorder::create(
                destination,
                config.name.as_str(),
                ".disp",
                policy,
                ts,
                recording.file_rotation,
            )
            .await
            .map(|r| start_display_recording(r, cap))
        };
        match created {
            Ok(recording) => Some((renderer, recording)),
            Err(_err) => {
                let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
                None
            }
        }
    }
}

/// Phase 2 of [`Listener::stop`]: await the taken-out handle's graceful shutdown.
/// Owns the handle (no `&mut Listener`), so a caller can wrap *this* in a timeout
/// and, if it fires, still run `Listener::finish_stop` — the cleanup never depends
/// on the drain completing.
async fn drain_handle(handle: Option<ChannelHandle>) {
    match handle {
        Some(ChannelHandle::Data(tasks)) => {
            let _ = tasks.stop().await;
        }
        Some(ChannelHandle::TcpListener(listener)) => listener.stop().await,
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{schema::InterfaceConfig, templates};

    fn udp_channel() -> ChannelConfig {
        // Template binds 0.0.0.0:0 (ephemeral) — binds cleanly in tests.
        templates::udp_template()
    }

    #[tokio::test]
    async fn remove_channel_drops_it_from_the_registry() {
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.remove_channel(id).await.unwrap();
        assert_eq!(listener.state(id), None, "the channel is gone");
        // Removing it again is an error (it's unknown now).
        assert!(listener.remove_channel(id).await.is_err());
    }

    #[tokio::test]
    async fn remove_running_channel_stops_then_drops_it() {
        let mut listener = Listener::with_default_capacities();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.remove_channel(id).await.unwrap();
        assert_eq!(listener.state(id), None);
    }

    #[tokio::test]
    async fn start_then_stop_drives_state_and_emits_events() {
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let id = listener.add_channel(udp_channel());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStarted(id)
        );

        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStopped(id)
        );
    }

    #[tokio::test]
    async fn illegal_transitions_are_rejected() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // Stop from Stopped is illegal.
        assert!(matches!(
            listener.stop(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));

        listener.start(id).await.unwrap();
        // Start from Running is illegal.
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));
        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn bad_config_faults_then_can_be_reset() {
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        let id = listener.add_channel(config);

        assert!(listener.start(id).await.is_err());
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelFaulted(id)
        );

        // Start from Faulted is illegal — it must be reset first (§9).
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));
        // Stop returns a Faulted channel to Stopped (§8.5).
        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn commit_and_start_recovers_a_faulted_channel_without_an_explicit_stop() {
        // A Faulted channel, fixed by committing a good config and starting in one
        // call — commit_and_start owns the Faulted→Stopped→Starting recovery, so the
        // caller never issues the intermediate Stop (the bug the GUI worked around).
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let mut bad = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut bad.interface {
            udp.bind_address = "not-an-ip-address".to_string();
        }
        let id = listener.add_channel(bad);
        assert!(listener.start(id).await.is_err());
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // A plain start from Faulted is still illegal directly...
        assert!(listener.start(id).await.is_err());
        // ...but commit_and_start with a good config recovers and comes up Running.
        listener
            .commit_and_start(id, Some(udp_channel()), true)
            .await
            .unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
    }

    #[tokio::test]
    async fn commit_and_start_restarts_a_running_channel_onto_the_new_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        // Commit a fresh (still-valid) config and restart in one call.
        listener
            .commit_and_start(id, Some(udp_channel()), true)
            .await
            .unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));
    }

    #[tokio::test]
    async fn stop_if_live_is_a_noop_on_a_stopped_channel() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // Stopped → no stop happened, and no IllegalTransition error to swallow.
        assert!(!listener.stop_if_live(id).await.unwrap());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));

        listener.start(id).await.unwrap();
        // Running → it stops.
        assert!(listener.stop_if_live(id).await.unwrap());
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn shutdown_lands_channels_in_stopped_and_emits_the_stop() {
        // shutdown times out only the drain, never the cleanup: every live channel
        // ends Stopped with a ChannelStopped emitted (see the cancel-vs-drain split).
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStarted(id)
        );

        listener.shutdown().await;
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
        assert_eq!(
            events.recv().await.unwrap(),
            RuntimeEvent::ChannelStopped(id)
        );
    }

    #[tokio::test]
    async fn apply_pending_restarts_with_the_new_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        // Accept a renamed config; the running channel is unaffected until applied.
        let mut updated = udp_channel();
        updated.name = crate::core::ChannelName::new("renamed");
        listener.set_pending_config(id, updated).unwrap();
        assert!(listener.has_pending(id));
        assert_eq!(listener.config(id).unwrap().name.as_str(), "UDP Channel");

        listener.apply_pending(id).await.unwrap();
        assert!(!listener.has_pending(id));
        assert_eq!(listener.config(id).unwrap().name.as_str(), "renamed");
        // Coordinated restart left it Running.
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.shutdown().await;
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn rename_takes_effect_immediately_without_restarting() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        listener
            .rename(id, crate::core::ChannelName::new("Bridge feed"))
            .unwrap();
        // The label changed and the channel kept Running — no coordinated restart.
        assert_eq!(listener.config(id).unwrap().name.as_str(), "Bridge feed");
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.shutdown().await;
    }

    #[test]
    fn rename_also_updates_a_queued_pending_config() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());

        // A pending edit is queued (e.g. a port change the user has not applied).
        let mut pending = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut pending.interface {
            udp.port = 9100;
        }
        listener.set_pending_config(id, pending).unwrap();

        // Renaming updates the live config and the pending one, so applying the
        // pending edit later does not revert the new name.
        listener
            .rename(id, crate::core::ChannelName::new("Renamed"))
            .unwrap();
        assert_eq!(listener.config(id).unwrap().name.as_str(), "Renamed");
        assert_eq!(listener.pending(id).unwrap().name.as_str(), "Renamed");
    }

    #[tokio::test]
    async fn display_views_can_be_paused_by_id() {
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        // The UDP template configures a Raw + Hex display → two views (§84).
        let id = listener.add_channel(udp_channel());
        assert!(listener.display_views(id).is_empty()); // none while Stopped

        listener.start(id).await.unwrap();
        let views = listener.display_views(id);
        assert_eq!(views.len(), 2);

        // Pause/resume a known view; an unknown view id errors.
        assert!(listener.pause_display(id, views[0]).is_ok());
        assert!(listener.resume_display(id, views[0]).is_ok());
        assert!(matches!(
            listener.pause_display(id, DisplayViewId::new()),
            Err(OrchestratorError::UnknownDisplayView(_))
        ));

        listener.stop(id).await.unwrap();
        assert!(listener.display_views(id).is_empty()); // cleared on stop
    }

    #[tokio::test]
    async fn spontaneous_fault_reconciles_state_then_clears_on_stop() {
        // ADR-006: the detached fault monitor can't mutate the orchestrator, so it
        // trips the shared flag. We trip it directly here (the exact signal the
        // monitor leaves) and assert the orchestrator reconciles without a command.
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        listener.channels[&id]
            .faulted
            .store(true, Ordering::Relaxed);

        // state() reads Faulted even though the stored lifecycle state is Running.
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
        // Command validation uses the reconciled state: Start is now illegal (§9).
        assert!(matches!(
            listener.start(id).await,
            Err(OrchestratorError::IllegalTransition { .. })
        ));

        // Stop recovers Faulted → Stopped (§8.5) and clears the flag.
        listener.stop(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    #[tokio::test]
    async fn auto_reconnect_restarts_a_faulted_channel() {
        // §162: with reconnect enabled, a faulted channel is re-Started after the
        // backoff when reconnect_tick is driven.
        use crate::config::ReconnectPolicy;
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        config.reconnect = ReconnectPolicy {
            enabled: true,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
            multiplier: 2.0,
            max_attempts: None,
        };
        let id = listener.add_channel(config);
        listener.start(id).await.unwrap();

        // Simulate a spontaneous fault (trip the flag, ADR-006).
        listener.channels[&id]
            .faulted
            .store(true, Ordering::Relaxed);
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // First tick only arms the backoff timer; the channel is still faulted.
        listener.reconnect_tick().await;
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));

        // After the backoff elapses, a tick reconnects it.
        tokio::time::sleep(Duration::from_millis(5)).await;
        listener.reconnect_tick().await;
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        let mut reconnected = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, RuntimeEvent::ChannelReconnected(_)) {
                reconnected = true;
            }
        }
        assert!(reconnected, "expected a ChannelReconnected event");

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn auto_reconnect_gives_up_after_max_attempts() {
        // §162: a channel that cannot start (bad config) gives up after the
        // configured number of attempts and stays faulted.
        use crate::config::ReconnectPolicy;
        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "not-an-ip-address".to_string(); // start always fails
        }
        config.reconnect = ReconnectPolicy {
            enabled: true,
            initial_backoff_ms: 1,
            max_backoff_ms: 5,
            multiplier: 1.0,
            max_attempts: Some(1),
        };
        let id = listener.add_channel(config);

        // The initial start fails → Faulted; reconnect then retries and gives up.
        assert!(listener.start(id).await.is_err());
        for _ in 0..6 {
            listener.reconnect_tick().await;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }

        let mut gave_up = false;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, RuntimeEvent::ChannelReconnectGaveUp(_)) {
                gave_up = true;
            }
        }
        assert!(gave_up, "expected a ChannelReconnectGaveUp event");
        assert_eq!(listener.state(id), Some(ChannelState::Faulted));
    }

    #[tokio::test]
    async fn serial_control_is_unavailable_on_non_serial_channels() {
        // §161: control-line commands/queries only apply to running serial
        // Channels; a UDP channel reports it has no serial control.
        let mut listener = Listener::with_default_capacities();
        let _ = listener.take_events();
        let id = listener.add_channel(udp_channel());
        listener.start(id).await.unwrap();

        assert!(matches!(
            listener.set_rts(id, true).await,
            Err(OrchestratorError::SerialControlUnavailable(_))
        ));
        assert!(listener.serial_control_lines(id).is_none());

        listener.stop(id).await.unwrap();
    }

    #[tokio::test]
    async fn unknown_channel_is_an_error() {
        let mut listener = Listener::with_default_capacities();
        let ghost = ChannelId::new();
        assert!(matches!(
            listener.start(ghost).await,
            Err(OrchestratorError::UnknownChannel(_))
        ));
    }

    #[tokio::test]
    async fn recording_enable_failure_does_not_fault_the_channel() {
        use crate::config::schema::RawRecordingConfig;
        use crate::record::OverwritePolicy;

        // Pre-create the destination so a Refuse policy makes enabling fail
        // (§55/§121).
        let mut path = std::env::temp_dir();
        path.push(format!("listener-orch-{}.bin", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"existing").await.unwrap();

        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        config.raw_recording = RawRecordingConfig {
            enabled: true,
            destination: Some(path.clone()),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Refuse,
            file_rotation: FileRotationPolicy::None,
            disk_guard: None,
        };
        let id = listener.add_channel(config);

        // Start succeeds: reception runs despite the failed recording enable.
        listener.start(id).await.unwrap();
        assert_eq!(listener.state(id), Some(ChannelState::Running));

        // A warning was surfaced for the recording-enable failure (§55).
        let mut saw_warning = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, RuntimeEvent::WarningRaised(_)) {
                saw_warning = true;
            }
        }
        assert!(
            saw_warning,
            "a WarningRaised event should surface the failure"
        );
        // The pre-existing file was not clobbered (§121).
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"existing");

        listener.stop(id).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
    }
}
