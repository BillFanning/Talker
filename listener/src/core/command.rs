//! Runtime event vocabulary the runtime orchestrator reports to the presentation
//! layers (spec §137).
//!
//! Events flow runtime → UI; the UI observes them and never owns transport,
//! recording, or pipeline state (§3). The *command* direction (UI → runtime) is
//! the [`Listener`](crate::runtime::Listener)'s async method API directly — there is
//! no separate runtime-command enum (ADR-012). The GUI's own `UiCommand`
//! (`gui::bridge`) is the on-the-wire form the driver translates into those calls.

use std::time::Duration;

use super::ids::{ChannelId, MatchRuleId};

/// Something the runtime reports happened (§137).
///
/// `#[non_exhaustive]`: the event vocabulary grows across versions, so observers
/// (notably the GUI's event-folding loop) must keep a wildcard arm and stay
/// forward-compatible.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeEvent {
    ChannelStarted(ChannelId),
    ChannelStopped(ChannelId),
    ChannelFaulted(ChannelId),
    RecordingFaulted(ChannelId),
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
    /// A sustained reader stall on a Channel's Transport→Pipeline edge (§101,
    /// ADR-007): possible transport-specific loss. Carries how long the reader was
    /// stalled. Dedicated variant (v1.2 §137) — replaces the earlier reuse of
    /// `WarningRaised` so observers can distinguish a stall from any other warning.
    ReceptionStalled(ChannelId, Duration),
    /// A serial Channel's control/status lines changed (§14.3, §161); read the
    /// current state from the snapshot/query. Part of the v1.2 §137 vocabulary.
    ControlLinesChanged(ChannelId),
    /// Auto-reconnect (§9.1, §162) is attempting to re-Start a faulted Channel;
    /// carries the 1-based attempt number.
    ChannelReconnecting(ChannelId, u32),
    /// Auto-reconnect succeeded — the Channel is Running again.
    ChannelReconnected(ChannelId),
    /// Auto-reconnect gave up after `max_attempts`; the Channel stays Faulted.
    ChannelReconnectGaveUp(ChannelId),
    /// Free disk space for a Channel's recording fell below its guard threshold
    /// (§56.2, §168). Recording may continue (Warn) or stop (see below).
    DiskSpaceLow(ChannelId),
    /// A recording was stopped and finalized because of low disk (§168); reception
    /// continues.
    RecordingStoppedLowDisk(ChannelId),
    /// A Match Rule fired on a Channel (§50.2, §165). Carries the runtime id of the
    /// rule that matched; `Notify`/`Mark` actions are observable here.
    MatchTriggered(ChannelId, MatchRuleId),
}
