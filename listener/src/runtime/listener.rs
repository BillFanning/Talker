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
//! surfaces a warning without faulting the Channel (§55). TCP **connection**
//! channels are not yet decoded or recorded.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::schema::InterfaceConfig;
use crate::config::ChannelConfig;
use crate::core::{ChannelId, ChannelState, DisplayViewId, RuntimeEvent};
use crate::display::{DisplayView, RenderedOutput};
use crate::record::{
    start_display_recording, start_raw_recording, DisplayFileRecorder, RawFileRecorder, Recording,
    RecordingMode,
};
use crate::transport::{DataTransportRunner, ReceivedData};

use super::build::{
    build_decoder, build_display_view, build_extractor, build_serial, build_tcp_listener,
    build_udp, BuildError,
};
use super::channel::{spawn_monitored_channel, MonitoredChannel};
use super::pipeline::{DisplayViewHandle, PipelineCapacities};
use super::tcp::{start_tcp_listener, TcpListenerHandle};

/// A live Channel's running tasks. Held by the orchestrator so it can stop them.
enum ChannelHandle {
    Data(MonitoredChannel),
    TcpListener(TcpListenerHandle),
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
            },
        );
        id
    }

    pub fn state(&self, id: ChannelId) -> Option<ChannelState> {
        self.channels.get(&id).map(|c| c.state)
    }

    pub fn config(&self, id: ChannelId) -> Option<&ChannelConfig> {
        self.channels.get(&id).map(|c| &c.config)
    }

    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.channels.keys().copied().collect()
    }

    pub fn has_pending(&self, id: ChannelId) -> bool {
        self.channels.get(&id).is_some_and(|c| c.pending.is_some())
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
            if !channel.state.can_transition_to(ChannelState::Starting) {
                return Err(OrchestratorError::IllegalTransition {
                    from: channel.state,
                    to: ChannelState::Starting,
                });
            }
            channel.config.clone()
        };

        self.set_state(id, ChannelState::Starting);
        match self.spawn_channel(id, &config).await {
            Ok(handle) => {
                let display_handles = match &handle {
                    ChannelHandle::Data(tasks) => tasks.display_handles().to_vec(),
                    // A TCP listener has no display itself; its connections do.
                    ChannelHandle::TcpListener(_) => Vec::new(),
                };
                if let Some(channel) = self.channels.get_mut(&id) {
                    channel.handle = Some(handle);
                    channel.display_handles = display_handles;
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
            ChannelState::Running => {
                self.set_state(id, ChannelState::Stopping);
                let handle = self.channels.get_mut(&id).and_then(|c| {
                    c.display_handles.clear();
                    c.handle.take()
                });
                match handle {
                    Some(ChannelHandle::Data(tasks)) => {
                        let _ = tasks.stop().await;
                    }
                    Some(ChannelHandle::TcpListener(listener)) => listener.stop().await,
                    None => {}
                }
                self.set_state(id, ChannelState::Stopped);
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelStopped(id));
                Ok(())
            }
            ChannelState::Faulted => {
                if let Some(channel) = self.channels.get_mut(&id) {
                    channel.display_handles.clear();
                }
                self.set_state(id, ChannelState::Stopped);
                let _ = self.events_tx.try_send(RuntimeEvent::ChannelStopped(id));
                Ok(())
            }
            other => Err(OrchestratorError::IllegalTransition {
                from: other,
                to: ChannelState::Stopped,
            }),
        }
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

    /// Stop every Running Channel (§113, application exit).
    pub async fn shutdown(&mut self) {
        let running: Vec<ChannelId> = self
            .channels
            .iter()
            .filter(|(_, c)| c.state == ChannelState::Running)
            .map(|(id, _)| *id)
            .collect();
        for id in running {
            let _ = self.stop(id).await;
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

    /// Build, open/bind, and wire a Channel's runtime tasks (§8.2).
    async fn spawn_channel(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
    ) -> Result<ChannelHandle, OrchestratorError> {
        match &config.interface {
            InterfaceConfig::Serial(serial) => {
                let opened = build_serial(id, serial)?
                    .open()
                    .await
                    .map_err(OrchestratorError::SerialOpen)?;
                let raw = self.build_raw_recorder(id, config).await;
                let display = self.build_display_recorder(id, config).await;
                Ok(ChannelHandle::Data(
                    self.spawn_data(id, opened, config, raw, display),
                ))
            }
            InterfaceConfig::Udp(udp) => {
                let bound = build_udp(id, udp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?;
                let raw = self.build_raw_recorder(id, config).await;
                let display = self.build_display_recorder(id, config).await;
                Ok(ChannelHandle::Data(
                    self.spawn_data(id, bound, config, raw, display),
                ))
            }
            InterfaceConfig::TcpListener(tcp) => {
                let bound = build_tcp_listener(id, tcp)?
                    .bind()
                    .await
                    .map_err(OrchestratorError::Bind)?;
                // Each accepted connection gets a fresh extractor from this config.
                // Per-connection recording/decoding is deferred (§16.2).
                let extraction = config.extraction.clone();
                let handle = start_tcp_listener(
                    bound,
                    move || build_extractor(&extraction),
                    self.channel_caps(config),
                    tcp.max_connections,
                    self.events_tx.clone(),
                );
                Ok(ChannelHandle::TcpListener(handle))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_data<R: DataTransportRunner>(
        &self,
        id: ChannelId,
        runner: R,
        config: &ChannelConfig,
        raw_recorder: Option<Recording<Arc<ReceivedData>>>,
        display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
    ) -> MonitoredChannel {
        spawn_monitored_channel(
            id,
            runner,
            build_extractor(&config.extraction),
            build_decoder(&config.decoder),
            raw_recorder,
            display_recorder,
            // One runtime Display View per configured view (§48); at least one.
            config.display.views.len(),
            self.channel_caps(config),
            self.events_tx.clone(),
        )
    }

    /// Create the Raw Recording handle for a Channel if enabled (§53). Per §55,
    /// failing to open the file (e.g. a refused overwrite, §121) does **not**
    /// fault the Channel: recording stays disabled, a warning is surfaced, and
    /// reception continues (§5.8). Returns `None` when disabled or on failure.
    async fn build_raw_recorder(
        &self,
        id: ChannelId,
        config: &ChannelConfig,
    ) -> Option<Recording<Arc<ReceivedData>>> {
        let recording = &config.recording;
        if recording.mode != RecordingMode::Raw {
            return None;
        }
        let Some(destination) = &recording.destination else {
            // Mode is Raw but no destination — cannot record; surface a warning.
            let _ = self.events_tx.try_send(RuntimeEvent::WarningRaised(id));
            return None;
        };
        match RawFileRecorder::create(
            destination,
            recording.overwrite_policy,
            recording.timestamp_enabled,
        )
        .await
        {
            Ok(recorder) => Some(start_raw_recording(recorder, self.caps.raw_recording)),
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
        match DisplayFileRecorder::create(
            destination,
            recording.overwrite_policy,
            recording.timestamp_enabled,
        )
        .await
        {
            Ok(recorder) => Some((
                renderer,
                start_display_recording(recorder, self.caps.raw_recording),
            )),
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
