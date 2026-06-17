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

/// One Channel's view-model: identity, derived status, the latest cheap liveness
/// facts, the most recent snapshot (diagnostics/matches) for detail panes, and the
/// incrementally-accumulated stream scrollback for the live viewer.
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
    /// The most recent snapshot, for detail panes (diagnostics, match firings,
    /// view pause, recording state). The stream bytes are *not* here — they
    /// accumulate separately in `stream_bytes` from incremental deltas. `None` until
    /// the first poll arrives.
    pub snapshot: Option<ChannelSnapshot>,
    /// Live serial control/status lines (§161) while running; `None` otherwise.
    pub control_lines: Option<SerialControlLines>,
    /// Raw-recording state from the latest snapshot/stats (§53), or `None` when no
    /// recorder is attached. Drives the recording indicator in the detail pane.
    pub recording: Option<RecordingState>,
    /// How many `BytePattern` matches were recovered across a read-chunk boundary
    /// (§50.2) — the cross-chunk-carry measurement. From the latest snapshot/stats;
    /// shown per-tab so it's visible even when the channel isn't selected.
    pub boundary_saves: u64,
    /// Accumulated stream scrollback bytes for the live viewer (§87, ADR-009),
    /// grown incrementally from [`UiUpdate::StreamDelta`] so the driver never
    /// re-ships the whole ~1 MB buffer each poll. Capped to `STREAM_VIEW_CAP`
    /// (oldest dropped) to match the runtime's bounded scrollback.
    pub stream_bytes: std::collections::VecDeque<u8>,
    /// Next absolute stream offset to request — the cursor handed to
    /// `Listener::stream_delta`. Advances as deltas are folded.
    pub stream_cursor: u64,
}

/// GUI-side scrollback cap (§87, §124): bounds the accumulated live-view bytes
/// independent of the runtime cap, so a long-lived selection can't grow the view
/// model without bound.
///
/// This is **display-only** — the live scroll-back window. It feeds nothing else
/// (recording is a separate pipeline tap; match rules and diagnostics don't read it),
/// so it only governs how far back you can scroll in the viewer. Kept small (~128 KB,
/// roughly 1000+ typical lines) so the viewer can soft-wrap every line each frame
/// without virtualization and still stay cheap; the full history lives in the `.raw`
/// recording, not here.
pub const STREAM_VIEW_CAP: usize = 128 * 1024;

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
            boundary_saves: 0,
            stream_bytes: std::collections::VecDeque::new(),
            stream_cursor: 0,
        }
    }

    /// Fold an incremental stream delta (§87, ADR-009) into the accumulated view
    /// bytes. Appends new bytes; if the runtime's window had evicted past our cursor
    /// (`base_offset` jumped ahead), reset to the returned window. Caps the buffer.
    fn apply_stream_delta(&mut self, base_offset: u64, bytes: &[u8], end_offset: u64) {
        // A base ahead of our cursor means our cursor was evicted: reset the view to
        // the returned window rather than appending a gap.
        if base_offset > self.stream_cursor {
            self.stream_bytes.clear();
        }
        self.stream_bytes.extend(bytes.iter().copied());
        self.stream_cursor = end_offset;
        let overflow = self.stream_bytes.len().saturating_sub(STREAM_VIEW_CAP);
        if overflow > 0 {
            self.stream_bytes.drain(..overflow);
        }
    }

    /// The accumulated stream bytes as a contiguous slice for rendering.
    pub fn stream_contiguous(&mut self) -> &[u8] {
        self.stream_bytes.make_contiguous()
    }
}

/// The GUI's whole model: Channels in registration order, folded from updates.
#[derive(Default)]
pub struct AppState {
    order: Vec<ChannelId>,
    views: HashMap<ChannelId, ChannelView>,
    /// A transient workspace-level status line — the result of the last profile
    /// Save/Load (e.g. "Saved profile.toml" or an error). `None` until one happens.
    workspace_status: Option<String>,
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

    /// Mutable access to a Channel's view — used by the detail pane to make its
    /// accumulated stream bytes contiguous for rendering (and to memoize rows).
    pub fn channel_mut(&mut self, id: ChannelId) -> Option<&mut ChannelView> {
        self.views.get_mut(&id)
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

    /// Channels that can be **started** right now — i.e. Stopped. Used by "Start all"
    /// so it skips channels already Running/Reconnecting (whose Start would be an
    /// illegal Running→Starting transition). Faulted is excluded here: clearing a
    /// fault is the per-channel "Retry" (Stop+Start), not a bulk start.
    pub fn startable_channel_ids(&self) -> Vec<ChannelId> {
        self.order
            .iter()
            .filter(|id| {
                self.views
                    .get(id)
                    .is_some_and(|v| v.status == ChannelStatus::Stopped)
            })
            .copied()
            .collect()
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
                    view.boundary_saves = snapshot.match_boundary_saves;
                    view.snapshot = Some(*snapshot);
                }
            }
            // Cheap per-tab health for non-selected channels (no scrollback bytes).
            UiUpdate::Stats(id, stats) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.bytes_total = stats.activity.total_bytes;
                    view.bytes_per_sec = stats.activity.bytes_per_sec;
                    view.info = stats.event_count;
                    view.warnings = stats.warning_count;
                    view.errors = stats.error_count;
                    view.recording = stats.raw_recording;
                    view.boundary_saves = stats.match_boundary_saves;
                }
            }
            UiUpdate::ControlLines(id, lines) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.control_lines = Some(lines);
                }
            }
            UiUpdate::StreamDelta(id, delta) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.apply_stream_delta(delta.base_offset, &delta.bytes, delta.end_offset);
                }
            }
            UiUpdate::ProfileSaved(path) => {
                let file = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("profile");
                self.workspace_status = Some(format!("Saved {file}"));
            }
            UiUpdate::ProfileLoaded(name) => {
                self.workspace_status = Some(format!("Loaded “{name}”"));
            }
            UiUpdate::ProfileError(message) => {
                self.workspace_status = Some(message);
            }
        }
    }

    /// The last profile Save/Load status line, if any (shown in the UI).
    pub fn workspace_status(&self) -> Option<&str> {
        self.workspace_status.as_deref()
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
                                            // A fresh Start resets the runtime's stream offset to 0, so drop
                                            // any accumulated bytes/cursor from a previous run to avoid mixing
                                            // old and new streams (§8.5).
                    view.stream_bytes.clear();
                    view.stream_cursor = 0;
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
            // A recording fault (e.g. a begin that couldn't open the file — Refuse over
            // an existing file) surfaces inline so "Record now" gives feedback instead
            // of silently doing nothing. The specific reason is in the diagnostics log.
            RuntimeEvent::RecordingFaulted(id) => {
                if let Some(view) = self.views.get_mut(&id) {
                    view.last_error = Some(
                        "recording could not start — see Diagnostics (check the \
                         destination and on-exists policy)"
                            .to_string(),
                    );
                }
            }
            // `MessageReceived` no longer drives the list: liveness is byte-based now
            // (ADR-009), refreshed by the periodic snapshot/stats poll like throughput.
            // Other warning, disk, control-line, match, and TCP-connection events are
            // reflected through the periodic snapshot (or land in later panes); the list
            // view does not need them directly. `RuntimeEvent` is `#[non_exhaustive]`,
            // so this wildcard also keeps us forward-compatible.
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
            stream_end_offset: total_bytes,
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
    fn snapshot_surfaces_cross_chunk_boundary_saves() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        // The cross-chunk measurement (§50.2) folds through to the per-tab view.
        let mut snap = snapshot_with(id, 100, 0.0, 0);
        snap.match_boundary_saves = 3;
        state.apply(UiUpdate::Snapshot(id, Box::new(snap)));
        assert_eq!(state.channel(id).unwrap().boundary_saves, 3);
    }

    fn delta(base: u64, bytes: &[u8], end: u64) -> crate::runtime::StreamDelta {
        crate::runtime::StreamDelta {
            base_offset: base,
            bytes: bytes.to_vec().into(),
            end_offset: end,
        }
    }

    #[test]
    fn stream_deltas_accumulate_incrementally() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"alpha", 5))));
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(5, b"bravo", 10))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"alphabravo");
        assert_eq!(view.stream_cursor, 10);
    }

    #[test]
    fn stream_delta_reset_on_eviction_replaces_rather_than_appends() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"old", 3))));
        // base_offset (10) jumped ahead of our cursor (3): the window was evicted, so
        // the view resets to the new window instead of leaving a gap.
        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(10, b"new", 13))));
        let view = state.channel_mut(id).unwrap();
        assert_eq!(view.stream_contiguous(), b"new");
        assert_eq!(view.stream_cursor, 13);
    }

    #[test]
    fn restart_clears_accumulated_stream() {
        let mut state = AppState::default();
        let id = ChannelId::new();
        state.apply(added(id, "udp", "UDP · test"));

        state.apply(UiUpdate::StreamDelta(id, Box::new(delta(0, b"before", 6))));
        // A fresh Start resets the runtime offset to 0; the view must drop the old
        // stream so post-restart bytes don't concatenate onto pre-restart ones.
        state.apply(UiUpdate::Event(RuntimeEvent::ChannelStarted(id)));
        let view = state.channel_mut(id).unwrap();
        assert!(view.stream_contiguous().is_empty());
        assert_eq!(view.stream_cursor, 0);
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
