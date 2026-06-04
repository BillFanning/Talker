//! Runtime command and event vocabulary exchanged between the presentation
//! layers and the runtime orchestrator (spec §136, §137).
//!
//! Commands flow UI → runtime; events flow runtime → UI. The UI issues commands
//! and observes events; it never owns transport, recording, or pipeline state
//! (§3).

use std::time::Duration;

use super::ids::{ChannelId, DisplayViewId, MatchRuleId};

/// A command directed at the runtime (§136).
///
/// `#[non_exhaustive]`: the v1.2 command surface is still growing (live serial
/// control, network adjustment, and match-rule commands land as the GUI's command
/// channel is built), so consumers must keep a wildcard arm.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuntimeCommand {
    StartChannel(ChannelId),
    StopChannel(ChannelId),
    /// Apply pending restart-required configuration via one coordinated
    /// stop/apply/start cycle (§13).
    ApplyPendingConfig(ChannelId),
    EnableRecording(ChannelId),
    DisableRecording(ChannelId),
    /// Pause a single Display View; other views keep running (§11).
    PauseDisplay(ChannelId, DisplayViewId),
    ResumeDisplay(ChannelId, DisplayViewId),
}

/// Something the runtime reports happened (§137). `MessageReceived` carries the
/// Channel-local Message Number (§24).
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
    MessageReceived(ChannelId, u64),
    RecordingFaulted(ChannelId),
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
    /// A sustained reader stall on a Channel's Transport→Extractor edge (§101,
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
