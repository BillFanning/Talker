//! The GUI↔runtime bridge (listener ADR-008).
//!
//! A background **driver** task owns the async [`Listener`] and stands between it
//! and the synchronous egui App. The App never touches the runtime directly
//! (AGENTS §5): it sends [`UiCommand`]s and receives [`UiUpdate`]s over channels,
//! and the driver translates commands into `Listener` method calls, forwards the
//! `RuntimeEvent` stream, and pushes periodic [`ChannelSnapshot`]s.
//!
//! This module is **egui-free** so it is unit-testable without a display: the only
//! coupling to the UI is an opaque `repaint` callback the driver invokes after
//! pushing an update (the App passes `egui::Context::request_repaint`).

use std::time::Duration;

use tokio::sync::mpsc::{Receiver, Sender};

use crate::config::{ChannelConfig, InterfaceConfig};
use crate::core::{ChannelId, ChannelName, DisplayViewId, RuntimeEvent};
use crate::runtime::{ChannelSnapshot, Listener, PipelineCapacities};
use crate::transport::udp::UdpMode;
use crate::transport::SerialControlLines;

/// How often the driver polls running Channels for a fresh snapshot (the pull
/// surface, ADR-006). 5 Hz is responsive without busy-polling.
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(200);
/// Auto-reconnect cadence (§162): the orchestrator has no background loop, so the
/// driver ticks it, exactly as the CLI does.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

/// A command from the GUI to the runtime (§136). Only commands with a backing
/// `Listener` method are modelled today; the dynamic in-pipeline commands
/// (`SetMatchRuleEnabled`, `MarkNow`, mid-run recording enable/disable) wait on the
/// command channel into `run_channel` (deferred, ADR-008).
#[derive(Debug)]
pub enum UiCommand {
    /// Register a channel from its config; the driver replies with `ChannelAdded`
    /// carrying the minted [`ChannelId`].
    AddChannel(Box<ChannelConfig>),
    Start(ChannelId),
    Stop(ChannelId),
    /// Remove a channel from the runtime entirely (stops it first if live). Used to
    /// recover from a misconfigured channel (e.g. a bind conflict).
    RemoveChannel(ChannelId),
    /// Replace a channel's configuration (e.g. change its port) via the §13
    /// pending-config/apply path: a Running channel restarts onto the new config;
    /// a Stopped/Faulted one swaps it in to be used on the next Start/Retry.
    Reconfigure(ChannelId, Box<ChannelConfig>),
    /// Rename a channel in place — instant, no restart (the name is a label only).
    Rename(ChannelId, ChannelName),
    ApplyPending(ChannelId),
    PauseDisplay(ChannelId, DisplayViewId),
    ResumeDisplay(ChannelId, DisplayViewId),
    SetRts(ChannelId, bool),
    SetDtr(ChannelId, bool),
    /// Stop all channels and end the driver (the App is closing).
    Shutdown,
}

/// An update from the runtime to the GUI. The App folds these into its view-model
/// ([`AppState`](super::state::AppState)); none of them borrow runtime state — each
/// carries owned data, so a slow UI can never stall reception.
#[derive(Debug)]
pub enum UiUpdate {
    /// A channel was registered: its runtime id, display name, a one-line
    /// connection description (interface + endpoint), and a copy of its config (so
    /// the UI can edit it, e.g. change the port).
    ChannelAdded(ChannelId, String, String, Box<ChannelConfig>),
    /// A channel's configuration changed (§13): its new connection description and
    /// config, so the UI refreshes its editor and details.
    ChannelReconfigured(ChannelId, String, Box<ChannelConfig>),
    /// A channel was renamed in place (§6) — its new display name. No restart and
    /// no connection change, so only the label updates.
    ChannelRenamed(ChannelId, String),
    /// A channel was removed from the runtime; the UI should drop it.
    ChannelRemoved(ChannelId),
    /// A command on a channel failed (e.g. a Start whose bind hit "address in
    /// use"): the channel id and the error text, so the UI can show the reason.
    ChannelError(ChannelId, String),
    /// A forwarded runtime event (the authoritative push surface, ADR-006).
    Event(RuntimeEvent),
    /// A periodic snapshot of a running channel (the pull surface).
    Snapshot(ChannelId, Box<ChannelSnapshot>),
    /// Current serial control/status lines for a running serial channel (§161),
    /// polled alongside snapshots.
    ControlLines(ChannelId, SerialControlLines),
}

/// A one-line, human-readable description of a channel's interface and endpoint,
/// for the connection-details line in the UI.
fn describe_interface(config: &ChannelConfig) -> String {
    match &config.interface {
        InterfaceConfig::Udp(udp) => {
            let mode = match udp.mode {
                UdpMode::Unicast => "unicast",
                UdpMode::Broadcast => "broadcast",
                UdpMode::Multicast => "multicast",
            };
            format!("UDP {mode} · bind {}:{}", udp.bind_address, udp.port)
        }
        InterfaceConfig::TcpListener(tcp) => {
            format!("TCP listener · {}:{}", tcp.bind_address, tcp.port)
        }
        InterfaceConfig::Serial(serial) => {
            format!("Serial · {} @ {} baud", serial.port, serial.baud_rate)
        }
    }
}

/// The background driver: owns the `Listener`, drains commands, forwards events,
/// and polls snapshots. Built by [`spawn`]; runs until `Shutdown` or the command
/// channel closes (the App dropped its sender).
pub struct Driver {
    listener: Listener,
    events: Receiver<RuntimeEvent>,
    commands: Receiver<UiCommand>,
    updates: Sender<UiUpdate>,
    repaint: Box<dyn Fn() + Send>,
    /// Channels registered so far, polled for snapshots.
    channels: Vec<ChannelId>,
}

impl Driver {
    /// Build a driver over an already-constructed `Listener` whose event stream has
    /// been taken. (`spawn` does this for the real GUI; tests build it directly.)
    pub fn new(
        listener: Listener,
        events: Receiver<RuntimeEvent>,
        commands: Receiver<UiCommand>,
        updates: Sender<UiUpdate>,
        repaint: Box<dyn Fn() + Send>,
    ) -> Self {
        Self {
            listener,
            events,
            commands,
            updates,
            repaint,
            channels: Vec::new(),
        }
    }

    /// Run until shutdown. Drains commands, forwards events, and on a timer polls
    /// each known channel for a snapshot and ticks auto-reconnect.
    pub async fn run(mut self) {
        let mut snapshot_tick = tokio::time::interval(SNAPSHOT_INTERVAL);
        snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut reconnect_tick = tokio::time::interval(RECONNECT_INTERVAL);
        reconnect_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut events_open = true;

        loop {
            tokio::select! {
                cmd = self.commands.recv() => match cmd {
                    Some(cmd) => {
                        if !self.handle(cmd).await {
                            break; // Shutdown
                        }
                    }
                    None => break, // the App dropped its command sender — close down
                },
                ev = self.events.recv(), if events_open => match ev {
                    Some(ev) => self.push(UiUpdate::Event(ev)),
                    None => events_open = false, // stream closed; keep serving commands
                },
                _ = snapshot_tick.tick() => self.poll_snapshots().await,
                _ = reconnect_tick.tick() => self.listener.reconnect_tick().await,
            }
        }

        // The App is gone (or asked to shut down): stop every channel cleanly.
        self.listener.shutdown().await;
    }

    /// Apply one command. Returns `false` only for `Shutdown` (end the loop).
    async fn handle(&mut self, cmd: UiCommand) -> bool {
        match cmd {
            UiCommand::AddChannel(config) => {
                let name = config.name.as_str().to_string();
                let details = describe_interface(&config);
                let echo = config.clone();
                let id = self.listener.add_channel(*config);
                self.channels.push(id);
                self.push(UiUpdate::ChannelAdded(id, name, details, echo));
            }
            UiCommand::Reconfigure(id, config) => {
                let config = *config;
                let details = describe_interface(&config);
                let _ = self.listener.set_pending_config(id, config.clone());
                match self.listener.apply_pending(id).await {
                    Ok(()) => {
                        self.push(UiUpdate::ChannelReconfigured(id, details, Box::new(config)))
                    }
                    Err(err) => self.push(UiUpdate::ChannelError(id, err.to_string())),
                }
            }
            UiCommand::Start(id) => {
                // Surface the reason on failure (e.g. a bind "address in use"),
                // instead of leaving the channel Faulted with no explanation.
                if let Err(err) = self.listener.start(id).await {
                    self.push(UiUpdate::ChannelError(id, err.to_string()));
                }
            }
            UiCommand::Stop(id) => {
                if let Err(err) = self.listener.stop(id).await {
                    self.push(UiUpdate::ChannelError(id, err.to_string()));
                }
            }
            UiCommand::RemoveChannel(id) => {
                let _ = self.listener.remove_channel(id).await;
                self.channels.retain(|c| *c != id);
                self.push(UiUpdate::ChannelRemoved(id));
            }
            UiCommand::Rename(id, name) => {
                if self.listener.rename(id, name.clone()).is_ok() {
                    self.push(UiUpdate::ChannelRenamed(id, name.as_str().to_string()));
                }
            }
            UiCommand::ApplyPending(id) => {
                if let Err(err) = self.listener.apply_pending(id).await {
                    self.push(UiUpdate::ChannelError(id, err.to_string()));
                }
            }
            UiCommand::PauseDisplay(id, view) => {
                let _ = self.listener.pause_display(id, view);
            }
            UiCommand::ResumeDisplay(id, view) => {
                let _ = self.listener.resume_display(id, view);
            }
            UiCommand::SetRts(id, on) => {
                let _ = self.listener.set_rts(id, on).await;
            }
            UiCommand::SetDtr(id, on) => {
                let _ = self.listener.set_dtr(id, on).await;
            }
            UiCommand::Shutdown => return false,
        }
        true
    }

    /// Poll every known channel for a snapshot (stopped/unknown yield `None`).
    /// Takes `&mut self` (not `&self`) so the `run` future stays `Send` — a shared
    /// `&Driver` held across the await would require `Driver: Sync`, which the mpsc
    /// `Receiver` is not. The real driver runs via `block_on` (no `Send` needed),
    /// but keeping it `Send` lets it run on a multi-thread runtime and be spawned.
    async fn poll_snapshots(&mut self) {
        for id in self.channels.clone() {
            if let Some(snap) = self.listener.snapshot(id).await {
                self.push(UiUpdate::Snapshot(id, Box::new(snap)));
            }
            // Live serial control/status lines (§161); `None` for non-serial or
            // stopped channels.
            if let Some(lines) = self.listener.serial_control_lines(id) {
                self.push(UiUpdate::ControlLines(id, lines));
            }
        }
    }

    /// Hand one update to the GUI and wake it. Advisory and non-blocking: a full
    /// update channel drops the item rather than stalling the driver (§99).
    fn push(&self, update: UiUpdate) {
        let _ = self.updates.try_send(update);
        (self.repaint)();
    }
}

/// A handle the egui App holds: the command sender, the update receiver, and the
/// runtime thread's join handle (kept alive for the App's lifetime).
pub struct BridgeHandle {
    pub commands: Sender<UiCommand>,
    pub updates: Receiver<UiUpdate>,
    _runtime_thread: std::thread::JoinHandle<()>,
}

/// Channel depths for the bridge. Commands are rare (user clicks); updates are
/// higher-volume (events + 5 Hz snapshots) but advisory.
const COMMAND_CAPACITY: usize = 64;
const UPDATE_CAPACITY: usize = 256;

/// Start the runtime on its own thread and return the App's [`BridgeHandle`].
///
/// `repaint` is invoked whenever an update is pushed; the App passes
/// `egui::Context::request_repaint` so a streaming source wakes the UI.
pub fn spawn(repaint: impl Fn() + Send + 'static) -> anyhow::Result<BridgeHandle> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(COMMAND_CAPACITY);
    let (upd_tx, upd_rx) = tokio::sync::mpsc::channel(UPDATE_CAPACITY);

    let runtime_thread = std::thread::Builder::new()
        .name("listener-runtime".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::error!("failed to start the async runtime: {err}");
                    return;
                }
            };
            runtime.block_on(async move {
                // A roomy recent-message buffer for scrollback — the message view
                // virtualizes (renders only visible rows), so a large ring is cheap
                // to display (#4). Other capacities stay at their defaults.
                let mut listener = Listener::new(PipelineCapacities {
                    display: 50_000,
                    retention: 50_000,
                    ..PipelineCapacities::default()
                });
                let events = listener
                    .take_events()
                    .expect("the event stream is available exactly once");
                Driver::new(listener, events, cmd_rx, upd_tx, Box::new(repaint))
                    .run()
                    .await;
            });
        })?;

    Ok(BridgeHandle {
        commands: cmd_tx,
        updates: upd_rx,
        _runtime_thread: runtime_thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{templates, InterfaceConfig};

    fn free_udp_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn udp_config(port: u16) -> ChannelConfig {
        let mut config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.bind_address = "127.0.0.1".to_string();
            udp.port = port;
        }
        config
    }

    /// The driver translates commands into runtime calls, forwards lifecycle events,
    /// and pushes snapshots — end to end over a loopback UDP channel.
    #[tokio::test]
    async fn driver_adds_starts_reports_and_snapshots_a_channel() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);

        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        // Helper: await the next update of interest within a timeout.
        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let port = free_udp_port();
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();

        // The driver mints the id and reports it, with connection details.
        let id = loop {
            if let UiUpdate::ChannelAdded(id, name, details, _) = next(&mut upd_rx).await {
                assert_eq!(name, "UDP Channel");
                assert!(
                    details.contains("UDP"),
                    "details name the interface: {details}"
                );
                break id;
            }
        };

        cmd_tx.send(UiCommand::Start(id)).await.unwrap();

        // Lifecycle event is forwarded…
        let mut started = false;
        let mut got_snapshot = false;
        // Send a datagram so a MessageReceived/snapshot has content.
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // Drain updates until we've seen both a start event and a snapshot.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !(started && got_snapshot) {
            if tokio::time::Instant::now() >= deadline {
                panic!("did not observe start + snapshot (started={started}, snap={got_snapshot})");
            }
            // Keep poking the channel so a datagram arrives after Start.
            let _ = client.send_to(b"hi", ("127.0.0.1", port)).await;
            match tokio::time::timeout(Duration::from_millis(250), upd_rx.recv()).await {
                Ok(Some(UiUpdate::Event(RuntimeEvent::ChannelStarted(eid)))) if eid == id => {
                    started = true;
                }
                Ok(Some(UiUpdate::Snapshot(sid, _))) if sid == id => got_snapshot = true,
                Ok(Some(_)) => {}
                Ok(None) => panic!("update stream closed early"),
                Err(_) => {} // tick again
            }
        }

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver did not shut down")
            .unwrap();
    }

    /// A Start that can't bind (two channels on one UDP port) reports the reason
    /// to the UI instead of leaving the channel Faulted with no explanation.
    #[tokio::test]
    async fn starting_two_channels_on_one_udp_port_reports_the_conflict() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(16);
        let (upd_tx, mut upd_rx) = tokio::sync::mpsc::channel(64);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        async fn next(rx: &mut Receiver<UiUpdate>) -> UiUpdate {
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver update timed out")
                .expect("update stream closed")
        }

        let port = free_udp_port();

        // First channel binds the port and starts.
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();
        let id1 = loop {
            if let UiUpdate::ChannelAdded(id, ..) = next(&mut upd_rx).await {
                break id;
            }
        };
        cmd_tx.send(UiCommand::Start(id1)).await.unwrap();
        loop {
            if let UiUpdate::Event(RuntimeEvent::ChannelStarted(e)) = next(&mut upd_rx).await {
                if e == id1 {
                    break;
                }
            }
        }

        // Second channel on the same port can't bind — the driver says why.
        cmd_tx
            .send(UiCommand::AddChannel(Box::new(udp_config(port))))
            .await
            .unwrap();
        let id2 = loop {
            if let UiUpdate::ChannelAdded(id, ..) = next(&mut upd_rx).await {
                if id != id1 {
                    break id;
                }
            }
        };
        cmd_tx.send(UiCommand::Start(id2)).await.unwrap();
        let reason = loop {
            if let UiUpdate::ChannelError(e, msg) = next(&mut upd_rx).await {
                if e == id2 {
                    break msg;
                }
            }
        };
        assert!(!reason.is_empty(), "the bind conflict reason is reported");

        cmd_tx.send(UiCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Dropping the command sender ends the driver (the App closed).
    #[tokio::test]
    async fn dropping_the_command_sender_stops_the_driver() {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<UiCommand>(4);
        let (upd_tx, _upd_rx) = tokio::sync::mpsc::channel(16);
        let mut listener = Listener::with_default_capacities();
        let events = listener.take_events().unwrap();
        let driver = Driver::new(listener, events, cmd_rx, upd_tx, Box::new(|| {}));
        let handle = tokio::spawn(driver.run());

        drop(cmd_tx); // the App is gone
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("driver did not stop when the command channel closed")
            .unwrap();
    }
}
