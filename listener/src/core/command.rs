//! Runtime command and event vocabulary exchanged between the presentation
//! layers and the runtime orchestrator (spec §136, §137).
//!
//! Commands flow UI → runtime; events flow runtime → UI. The UI issues commands
//! and observes events; it never owns transport, recording, or pipeline state
//! (§3).

use super::ids::{ChannelId, DisplayViewId};

/// A command directed at the runtime (§136).
#[derive(Clone, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeEvent {
    ChannelStarted(ChannelId),
    ChannelStopped(ChannelId),
    ChannelFaulted(ChannelId),
    MessageReceived(ChannelId, u64),
    RecordingFaulted(ChannelId),
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
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
}
