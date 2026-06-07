//! The runtime orchestrator (spec §10, §13, §97.1, §113).
//!
//! [`Listener`] owns the Channel registry and drives the [`ChannelState`]
//! lifecycle (§8/§9): it builds live transports/extractors/decoders from
//! validated config (via [`super::build`]), opens/binds them at Start (resource
//! failures → `Faulted`, §71/§8.2), wires the pipeline, and stops them on
//! request. All channels share one [`RuntimeEvent`] stream (§137).
//!
//! Commands are exposed as async methods (`start`/`stop`/`apply_pending`),
//! the vocabulary of [`crate::core::RuntimeCommand`]. Raw recording is wired for
//! serial/UDP channels from `RecordingConfig`; a recording-enable failure
//! surfaces a warning without faulting the Channel (§55). Accepted TCP
//! **connection** channels inherit the listener's extraction and decoder (§16.2);
//! per-connection recording and snapshots remain deferred (§59 filename
//! templates; the supervisor keeps no per-connection handle).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::config::schema::InterfaceConfig;
use crate::config::{ChannelConfig, Subsample};
use crate::core::{ChannelId, ChannelState, DisplayViewId, RuntimeEvent};
use crate::display::{DisplayView, RenderedOutput};
use crate::record::{
    start_display_recording, start_raw_recording, DisplayFileRecorder, FileRotationPolicy,
    RawFileRecorder, Recording, RecordingMode, RotatingDisplayRecorder, RotatingRawRecorder,
};
use crate::transport::{
    DataTransportRunner, SerialControlCommand, SerialControlHooks, SerialControlLines,
    TransportNotice,
};

use super::build::{
    build_decoder, build_display_view, build_extractor, build_serial, build_tcp_listener,
    build_udp, BuildError,
};
use super::channel::{
    spawn_monitored_channel, DataRecorder, MatchSetup, MonitoredChannel, TRANSPORT_NOTICES,
};
use super::pipeline::{DisplayViewHandle, PipelineCapacities, RawRecordArming};
use super::snapshot::ChannelSnapshot;
use super::tcp::{start_tcp_listener, TcpListenerHandle};

/// Per-view subsampling policies for a Channel (§50.1), in view order. At least
/// one entry, so the pipeline's default Display View is always covered.
fn view_subsamples(config: &ChannelConfig) -> Vec<Subsample> {
    if config.display.views.is_empty() {
        vec![Subsample::None]
    } else {
        config.display.views.iter().map(|v| v.subsample).collect()
    }
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
/// the lifecycle state, and the running handle (when Running).
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

    /// Request an on-demand snapshot of a running Channel's pipeline state (§137,
    /// ADR-006): retained Messages with decoder annotations, per-view display
    /// history, diagnostics, and recording state. Returns `None` when the Channel
    /// is unknown, not running, or a TCP listener (its connections are snapshot
    /// targets in their own right; per-connection snapshots are deferred). The
    /// `RuntimeEvent` stream stays the authoritative liveness signal.
    pub async fn snapshot(&self, id: ChannelId) -> Option<ChannelSnapshot> {
        match self.channels.get(&id)?.handle.as_ref()? {
            ChannelHandle::Data(tasks) => tasks.snapshot().await,
            ChannelHandle::TcpListener(_) => None,
        }
    }

    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.keys().copied().collect()
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
    pub async fn stop(&mut self, id: ChannelId) -> Result<(), OrchestratorError> {
        let state = self
            .state(id)
            .ok_or(OrchestratorError::UnknownChannel(id))?;
        match state {
            // Running → graceful drain; Faulted → §8.5 recovery. Both consume the
            // handle (a spontaneous fault leaves one whose tasks have already
            // ended; a start-time fault leaves none) and land in Stopped.
            ChannelState::Running | ChannelState::Faulted => {
                if state == ChannelState::Running {
                    self.set_state(id, ChannelState::Stopping);
                }
                let handle = self.channels.get_mut(&id).and_then(|c| c.handle.take());
                match handle {
                    Some(ChannelHandle::Data(tasks)) => {
                        let _ = tasks.stop().await;
                    }
                    Some(ChannelHandle::TcpListener(listener)) => listener.stop().await,
                    None => {}
                }
                self.finish_stop(id);
                Ok(())
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
            let _ = self.stop(id).await;
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
            retention: retention.message_limit.unwrap_or(self.caps.retention),
            retention_bytes: retention.byte_limit,
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
                // Each accepted connection gets a fresh extractor + decoder from
                // this config (§16.2). Per-connection recording is deferred (§59).
                let extraction = config.extraction.clone();
                let decoder = config.decoder.clone();
                let handle = start_tcp_listener(
                    bound,
                    move || build_extractor(&extraction),
                    move || build_decoder(&decoder),
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
            build_extractor(&config.extraction),
            build_decoder(&config.decoder),
            data_recorder,
            display_recorder,
            // One runtime Display View per configured view (§48), each with its
            // own subsampling policy (§50.1); at least one (default, no subsample).
            view_subsamples(config),
            // Disk-space guard (§56.2, §168): only when both a guard and a
            // recording destination are configured.
            config
                .recording
                .disk_guard
                .zip(config.recording.destination.clone()),
            // Match Rules (§50.2, §165) plus arming for any match-triggered `Record`
            // (lazy-create from the configured destination — nothing until a match).
            MatchSetup {
                rules: config.match_rules.clone(),
                arming: self.record_arming(config),
            },
            self.channel_caps(config),
            self.events_tx.clone(),
            faulted,
            notices_rx,
        )
    }

    /// Arming for a match-triggered `Record` action (§50.2): the bits needed to
    /// lazily build the Channel's Raw/`.ssdat` recording on a `Begin`, present only
    /// when a recording destination is configured. Mirrors `build_raw_recorder`'s
    /// destination/overwrite/timestamp/rotation/subsample choices so a rule-armed
    /// recording matches what static recording would have produced.
    fn record_arming(&self, config: &ChannelConfig) -> Option<RawRecordArming> {
        config
            .recording
            .destination
            .clone()
            .map(|destination| RawRecordArming {
                destination,
                channel_name: config.name.as_str().to_string(),
                overwrite: config.recording.overwrite_policy,
                timestamps: config.recording.timestamp_enabled,
                file_rotation: config.recording.file_rotation,
                subsample: config.recording.subsample,
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
        let recording = &config.recording;
        if recording.mode != RecordingMode::Raw {
            return None;
        }
        let Some(destination) = &recording.destination else {
            // Mode is Raw but no destination — cannot record; surface a warning.
            let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
            return None;
        };
        let policy = recording.overwrite_policy;
        let ts = recording.timestamp_enabled;
        let cap = self.caps.raw_recording;
        // A subsampled data recording is message-framed `.ssdat` (§50.1/§53), fed
        // per-Message and decimated; a plain Raw recording is byte-exact `.dat`.
        let subsampled = recording.subsample != Subsample::None;
        let ext = if subsampled { ".ssdat" } else { ".dat" };
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
                ext,
                policy,
                ts,
                recording.file_rotation,
            )
            .await
            .map(|r| start_raw_recording(r, cap))
        };
        match created {
            Ok(rec) if subsampled => Some(DataRecorder::Subsampled(rec, recording.subsample)),
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
        let recording = &config.recording;
        if recording.mode != RecordingMode::Display {
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
        use crate::config::schema::RecordingConfig;
        use crate::record::{OverwritePolicy, RecordingMode};

        // Pre-create the destination so a Refuse policy makes enabling fail
        // (§55/§121).
        let mut path = std::env::temp_dir();
        path.push(format!("listener-orch-{}.bin", uuid::Uuid::new_v4()));
        tokio::fs::write(&path, b"existing").await.unwrap();

        let mut listener = Listener::with_default_capacities();
        let mut events = listener.take_events().unwrap();
        let mut config = udp_channel();
        config.recording = RecordingConfig {
            mode: RecordingMode::Raw,
            destination: Some(path.clone()),
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::Refuse,
            file_rotation: FileRotationPolicy::None,
            subsample: Subsample::None,
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
