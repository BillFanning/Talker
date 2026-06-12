//! The GUI view-model and its reducer (listener ADR-008).
//!
//! [`AppState`] is the egui App's entire model: a per-Channel [`ChannelView`]
//! folded from the [`UiUpdate`] stream. It is **pure** — no egui, no runtime — so
//! the fold is unit-tested directly. The App calls [`AppState::apply`] for each
//! update it drains, then lays widgets out by reading the resulting views.

use std::collections::HashMap;

use crate::config::ChannelConfig;
use crate::core::{ChannelId, RecordingState, RuntimeEvent};
use crate::runtime::ChannelSnapshot;
use crate::transport::SerialControlLines;

use super::bridge::UiUpdate;

/// A Channel's lifecycle as the GUI understands it, derived from the event stream
/// (the authoritative push surface, ADR-006).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelStatus {
    #[default]
    Stopped,
    Running,
    Faulted,
    Reconnecting,
}

/// One Channel's view-model: identity, derived status, and the latest cheap
/// liveness facts, plus the most recent full snapshot for detail panes.
pub struct ChannelView {
    pub id: ChannelId,
    pub name: String,
    /// A one-line connection description (interface + endpoint).
    pub details: String,
    /// The channel's configuration, so the UI can edit it (e.g. change the port).
    /// Updated on reconfigure.
    pub config: ChannelConfig,
    pub status: ChannelStatus,
    /// Total bytes received since start (the Stream-oriented liveness counter — §18,
    /// ADR-009; Stream Mode has no Message count). From the latest snapshot/stats.
    pub bytes_total: u64,
    /// Rolling throughput from the latest snapshot (§91.1, §166).
    pub bytes_per_sec: f64,
    /// Retained per-severity diagnostic counts from the latest snapshot (§88), shown
    /// on the channel tab (#8).
    pub info: usize,
    pub warnings: usize,
    pub errors: usize,
    /// The reason the last command on this Channel failed (e.g. a bind conflict),
    /// cleared on a successful (re)start. `None` when there's nothing to report.
    pub last_error: Option<String>,
    /// The most recent full snapshot, for detail panes (retained Messages, hex,
    /// diagnostics, match firings). `None` until the first poll arrives.
    pub snapshot: Option<ChannelSnapshot>,
    /// Live serial control/status lines (§161) while running; `None` otherwise.
    pub control_lines: Option<SerialControlLines>,
    /// Raw-recording state from the latest snapshot/stats (§53), or `None` when no
    /// recorder is attached. Drives the recording indicator in the detail pane.
    pub recording: Option<RecordingState>,
}

impl ChannelView {
    fn new(id: ChannelId, name: String, details: String, config: ChannelConfig) -> Self {
        Self {
            id,
            name,
            details,
            config,
            status: ChannelStatus::Stopped,
            bytes_total: 0,
            bytes_per_sec: 0.0,
            info: 0,
            warnings: 0,
            errors: 0,
            last_error: None,
            snapshot: None,
            control_lines: None,
            recording: None,
        }
    }
}

/// The GUI's whole model: Channels in registration order, folded from updates.
#[derive(Default)]
pub struct AppState {
    order: Vec<ChannelId>,
    views: HashMap<ChannelId, ChannelView>,
}

impl AppState {
    /// The Channels in registration order (for a stable list).
    pub fn channels(&self) -> impl Iterator<Item = &ChannelView> {
        self.order.iter().filter_map(|id| self.views.get(id))
    }

    /// Look up one Channel's view-model.
    pub fn channel(&self, id: ChannelId) -> Option<&ChannelView> {
        self.views.get(&id)
    }

    /// The Channel adjacent to `id` in list order: the one above it, or — if `id` is
    /// first — the one below. `None` if `id` is unknown or the only Channel. Used to
    /// keep a selection after the selected Channel is removed (#2).
    pub fn neighbor(&self, id: ChannelId) -> Option<ChannelId> {
        let pos = self.order.iter().position(|c| *c == id)?;
        if pos > 0 {
            self.order.get(pos - 1).copied()
        } else {
            self.order.get(pos + 1).copied()
        }
    }

    /// The Channel before/after `id` in list order, wrapping around — for keyboard
    /// tab-cycling (#3). `None` if `id` is unknown.
    pub fn cycle(&self, id: ChannelId, forward: bool) -> Option<ChannelId> {
        let n = self.order.len();
        if n == 0 {
            return None;
        }
        let pos = self.order.iter().position(|c| *c == id)?;
        let next = if forward {
            (pos + 1) % n
        } else {
            (pos + n - 1) % n
        };
        self.order.get(next).copied()
    }

    /// The first Channel in list order, if any (for selecting something sensible
    /// when nothing is focused).
    pub fn first(&self) -> Option<ChannelId> {
        self.order.first().copied()
    }

    /// All Channel ids in list order (for bulk Start all / Stop all, #5).
    pub fn channel_ids(&self) -> Vec<ChannelId> {
        self.order.clone()
    }

    /// Fold one update into the model.
    pub fn apply(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::ChannelAdded(id, name, details, config) => {
                if !self.views.contains_key(&id) {
                    self.order.push(id);
                    self.views
                        .insert(id, ChannelView::new(id, name, details, *config));
                }
            }
            UiUpdate::ChannelReconfigured(id, details, config) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.details = details;
                    // Mirror a renamed channel into the list row (#6).
                    view.name = config.name.as_str().to_string();
                    view.config = *config;
                }
            }
            UiUpdate::ChannelRenamed(id, name) => {
                if let Some(view) = self.views.get_mut(&id) {
                    // Keep both the list label and the editor's seed config in step,
                    // so re-opening the editor shows the new name (§6).
                    view.config.name = crate::core::ChannelName::new(name.clone());
                    view.name = name;
                }
            }
            UiUpdate::ChannelRemoved(id) => {
                self.views.remove(&id);
                self.order.retain(|c| *c != id);
            }
            UiUpdate::ChannelError(id, message) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.last_error = Some(message);
                }
            }
            UiUpdate::Event(event) => self.apply_event(event),
            UiUpdate::Snapshot(id, snapshot) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.bytes_total = snapshot.activity.total_bytes;
                    view.bytes_per_sec = snapshot.activity.bytes_per_sec;
                    view.info = snapshot.diagnostics.events.len();
                    view.warnings = snapshot.diagnostics.warnings.len();
                    view.errors = snapshot.diagnostics.errors.len();
                    view.recording = snapshot.raw_recording;
                    view.snapshot = Some(*snapshot);
                }
            }
            // Cheap per-tab health for non-selected channels (no message history).
            UiUpdate::Stats(id, stats) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.bytes_total = stats.activity.total_bytes;
                    view.bytes_per_sec = stats.activity.bytes_per_sec;
                    view.info = stats.event_count;
                    view.warnings = stats.warning_count;
                    view.errors = stats.error_count;
                    view.recording = stats.raw_recording;
                }
            }
            UiUpdate::ControlLines(id, lines) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.control_lines = Some(lines);
                }
            }
        }
    }

    /// Fold a forwarded `RuntimeEvent`. Events for Channels we have not registered
    /// (e.g. runtime-minted TCP connection channels, §16.4) are ignored for now —
    /// the list shows configured Channels; per-connection views come later.
    fn apply_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::ChannelStarted(id) => {
                self.set_status(id, ChannelStatus::Running);
                if let Some(view) = self.views.get_mut(&id) {
                    view.last_error = None; // a successful start clears the prior error
                }
            }
            RuntimeEvent::ChannelStopped(id) => {
                self.set_status(id, ChannelStatus::Stopped);
                if let Some(view) = self.views.get_mut(&id) {
                    view.control_lines = None; // no live lines while stopped
                }
            }
            RuntimeEvent::ChannelFaulted(id) => self.set_status(id, ChannelStatus::Faulted),
            RuntimeEvent::ChannelReconnecting(id, _) => {
                self.set_status(id, ChannelStatus::Reconnecting)
            }
            RuntimeEvent::ChannelReconnected(id) => self.set_status(id, ChannelStatus::Running),
            RuntimeEvent::ChannelReconnectGaveUp(id) => self.set_status(id, ChannelStatus::Faulted),
            // `MessageReceived` no longer drives the list: liveness is byte-based now
            // (ADR-009), refreshed by the periodic snapshot/stats poll like throughput.
            // Warnings, recording, disk, control-line, match, and TCP-connection
            // events are reflected through the periodic snapshot (or land in later
            // panes); the list view does not need them directly. `RuntimeEvent` is
            // `#[non_exhaustive]`, so this wildcard also keeps us forward-compatible.
            _ => {}
        }
    }

    fn set_status(&mut self, id: ChannelId, status: ChannelStatus) {
        if let Some(view) = self.views.get_mut(&id) {
            view.status = status;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::templates;
    use crate::runtime::snapshot::{ChannelSnapshot, DiagnosticsSnapshot};
    use crate::runtime::ChannelActivity;

    /// A `ChannelAdded` update with a throwaway config (the reducer tests don't
    /// inspect the config; the editor uses it).
    fn added(id: ChannelId, name: &str, details: &str) -> UiUpdate {
        UiUpdate::ChannelAdded(
            id,
            name.into(),
            details.into(),
            Box::new(templates::udp_template()),
        )
    }

    fn snapshot_with(
        id: ChannelId,
        total_bytes: u64,
        bps: f64,
        warnings: usize,
    ) -> ChannelSnapshot {
        ChannelSnapshot {
            channel_id: id,
            display_views: vec![],
            diagnostics: DiagnosticsSnapshot {
                warnings: vec![crate::diagnostics::Diagnostic::warning("w"); warnings],
                ..DiagnosticsSnapshot::default()
            },
            raw_recording: None,
            activity: ChannelActivity {
                last_data_at: None,
                bytes_per_sec: bps,
                total_bytes,
            },
            matches: vec![],
            match_boundary_saves: 0,
            stream_tail: Vec::new().into(),
        }
    }

    #[test]
    fn folds_registration_and_lifecycle() {
        let mut state = AppState::default();
        let id = ChannelId::new();

        state.apply(added(id, "udp", "UDP · test"));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Stopped);
        assert_eq!(state.channels().count(), 1);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelFaulted(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Faulted);
    }

    #[test]
    fn reconnect_events_drive_status() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "serial", "Serial · COM3"));

        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnecting(id, 1)));
        assert_eq!(
            state.channel(id).unwrap().status,
            ChannelStatus::Reconnecting
        );
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnected(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelReconnectGaveUp(id)));
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Faulted);
    }

    #[test]
    fn snapshot_updates_liveness_and_is_retained() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::Snapshot(
            id,
            Box::new(snapshot_with(id, 4096, 42.0, 2)),
        ));
        let view = state.channel(id).unwrap();
        assert_eq!(view.bytes_total, 4096);
        assert_eq!(view.bytes_per_sec, 42.0);
        assert_eq!(view.warnings, 2);
        assert!(view.snapshot.is_some());
    }

    #[test]
    fn channel_error_is_recorded_and_cleared_on_a_successful_start() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · :9000"));
        assert!(state.channel(id).unwrap().last_error.is_none());
        assert_eq!(state.channel(id).unwrap().details, "UDP · :9000");

        // A failed start surfaces the reason.
        state.apply(UiUpdate::ChannelError(
            id,
            "failed to bind: address in use".into(),
        ));
        assert_eq!(
            state.channel(id).unwrap().last_error.as_deref(),
            Some("failed to bind: address in use")
        );

        // A later successful start clears it.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        assert!(state.channel(id).unwrap().last_error.is_none());
        assert_eq!(state.channel(id).unwrap().status, ChannelStatus::Running);
    }

    #[test]
    fn channel_reconfigured_updates_details_and_config() {
        use crate::config::InterfaceConfig;
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · :9000"));

        let mut new_config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut new_config.interface {
            udp.port = 9100;
        }
        state.apply(UiUpdate::ChannelReconfigured(
            id,
            "UDP · :9100".into(),
            Box::new(new_config),
        ));

        let view = state.channel(id).unwrap();
        assert_eq!(view.details, "UDP · :9100");
        match &view.config.interface {
            InterfaceConfig::Udp(udp) => assert_eq!(udp.port, 9100),
            other => panic!("expected UDP, got {other:?}"),
        }
    }

    #[test]
    fn channel_removed_drops_it_from_the_model() {
        let mut state = AppState::default();
        let a = ChannelId::new();
        let b = ChannelId::new();
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));

        state.apply(UiUpdate::ChannelRemoved(a));
        assert_eq!(state.channels().count(), 1);
        assert!(state.channel(a).is_none());
        assert!(state.channel(b).is_some());
    }

    #[test]
    fn events_for_unknown_channels_are_ignored() {
        let mut state = AppState::default();
        let known = ChannelId::new();
        state.apply(added(known, "a", "UDP"));

        // A runtime-minted (e.g. TCP connection) id we never registered.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(
            ChannelId::new(),
        )));
        assert_eq!(state.channels().count(), 1);
        // The known channel is untouched.
        assert_eq!(state.channel(known).unwrap().status, ChannelStatus::Stopped);
    }

    #[test]
    fn registration_order_is_stable() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));
        // A duplicate add does not reorder or duplicate.
        state.apply(added(a, "a-again", "UDP"));

        let names: Vec<&str> = state.channels().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn neighbor_picks_the_channel_above_then_below() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));

        // Middle/last fall back to the one above; the first falls back to the next.
        assert_eq!(state.neighbor(b), Some(a));
        assert_eq!(state.neighbor(c), Some(b));
        assert_eq!(state.neighbor(a), Some(b));

        // The sole remaining channel has no neighbor.
        state.apply(UiUpdate::ChannelRemoved(b));
        state.apply(UiUpdate::ChannelRemoved(c));
        assert_eq!(state.neighbor(a), None);
        assert_eq!(state.neighbor(ChannelId::new()), None); // unknown
    }

    #[test]
    fn cycle_wraps_in_both_directions() {
        let mut state = AppState::default();
        let (a, b, c) = (ChannelId::new(), ChannelId::new(), ChannelId::new());
        state.apply(added(a, "a", "UDP"));
        state.apply(added(b, "b", "TCP"));
        state.apply(added(c, "c", "Serial"));

        assert_eq!(state.cycle(a, true), Some(b));
        assert_eq!(state.cycle(c, true), Some(a)); // wrap forward
        assert_eq!(state.cycle(a, false), Some(c)); // wrap back
        assert_eq!(state.first(), Some(a));
    }

    #[test]
    fn renamed_updates_both_the_label_and_the_editor_seed() {
        let mut state = AppState::default();
        let a = ChannelId::new();
        state.apply(added(a, "a", "UDP"));

        state.apply(UiUpdate::ChannelRenamed(a, "Bridge".into()));
        let v = state.channel(a).unwrap();
        assert_eq!(v.name, "Bridge");
        assert_eq!(v.config.name.as_str(), "Bridge");
    }
}
