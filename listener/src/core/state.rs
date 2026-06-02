//! Channel kinds and the lifecycle/state enums (spec §7, §8, §11, §12, §130),
//! plus the §9 state-transition rules.

/// The four supported Channel kinds (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Serial,
    Udp,
    TcpListener,
    TcpConnection,
}

/// Channel lifecycle state (§8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Faulted,
}

impl ChannelState {
    /// Whether a direct transition `self -> next` is permitted by §9.
    ///
    /// Permitted edges (§9):
    /// `Stopped → Starting → Running`, `Running → Stopping → Stopped`,
    /// `Starting → Faulted`, `Running → Faulted`, `Faulted → Stopped`.
    ///
    /// Everything else — including the explicitly forbidden
    /// `Stopped → Running`, `Running → Starting`, `Faulted → Running`, and any
    /// self-transition — is rejected.
    pub fn can_transition_to(self, next: ChannelState) -> bool {
        use ChannelState::*;
        matches!(
            (self, next),
            (Stopped, Starting)
                | (Starting, Running)
                | (Running, Stopping)
                | (Stopping, Stopped)
                | (Starting, Faulted)
                | (Running, Faulted)
                | (Faulted, Stopped)
        )
    }
}

/// Per-view display presentation state (§11). Runtime-only; never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayState {
    Active,
    Paused,
}

/// Recording state, independent of Channel state (§12). A Channel may be
/// `Running` while recording is `Faulted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingState {
    Disabled,
    Enabled,
    Faulted,
}

#[cfg(test)]
mod tests {
    use super::ChannelState::*;

    #[test]
    fn permitted_transitions_are_allowed() {
        assert!(Stopped.can_transition_to(Starting));
        assert!(Starting.can_transition_to(Running));
        assert!(Running.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Stopped));
        assert!(Starting.can_transition_to(Faulted));
        assert!(Running.can_transition_to(Faulted));
        assert!(Faulted.can_transition_to(Stopped));
    }

    #[test]
    fn explicitly_forbidden_transitions_are_rejected() {
        // §9 names these three as forbidden direct transitions.
        assert!(!Stopped.can_transition_to(Running));
        assert!(!Running.can_transition_to(Starting));
        assert!(!Faulted.can_transition_to(Running));
    }

    #[test]
    fn self_transitions_are_rejected() {
        for state in [Stopped, Starting, Running, Stopping, Faulted] {
            assert!(
                !state.can_transition_to(state),
                "{state:?} should not transition to itself"
            );
        }
    }

    #[test]
    fn stopping_and_stopped_cannot_fault_directly() {
        // §9 lists Faulted only from Starting and Running.
        assert!(!Stopping.can_transition_to(Faulted));
        assert!(!Stopped.can_transition_to(Faulted));
    }
}
