//! What the GUI keeps about one channel, and the runtime does not.
//!
//! Per-channel view state belongs in this one struct rather than in a vector
//! per field. Five lifecycle paths (new profile, load, add, remove, start)
//! maintain the positional correspondence with the supervisor's slots, and a
//! vector that falls out of step hands out another channel's state rather than
//! panicking. A field added here is created, cleared, removed, and reset with
//! its siblings because there is nowhere else for it to be.
//!
//! What stays outside is the state the *profile* owns — interface and message
//! drafts, and the analysis cache keyed to them — which is edited on a
//! different schedule and is not view state.

use super::display::ChannelDisplay;
use super::notice::ChannelNotices;
use super::RateTracker;

/// One channel's GUI-side view state.
pub(super) struct ChannelView {
    /// The Output pane's buffer: sampled payloads as they were sent.
    pub(super) display: ChannelDisplay,
    /// Warnings this reader has already acknowledged.
    pub(super) notices: ChannelNotices,
    /// Rolling five-second acceptance rate for the channel row.
    pub(super) rate: RateTracker,
}

impl Default for ChannelView {
    fn default() -> Self {
        Self {
            display: ChannelDisplay::default(),
            notices: ChannelNotices::default(),
            // Not derived: the tracker's window is anchored to an epoch, and
            // `Instant` has no default to anchor it to.
            rate: RateTracker::new(),
        }
    }
}

impl ChannelView {
    /// Forget what a fresh run invalidates.
    ///
    /// Every per-run reset in one place, for the same reason the fields are:
    /// the rate window is meaningless across a restart, the pane's run state is
    /// stale, and a fresh run has nothing yet acknowledged. The last of those is
    /// belt and braces — [`super::notice::DismissedNotice`] re-arms itself when
    /// a counter falls below its record — but the guarantee should not rest on
    /// a self-correction that lives two modules away.
    pub(super) fn start_run(&mut self) {
        self.display.reset_run_state();
        self.notices = ChannelNotices::default();
        self.rate = RateTracker::new();
    }
}
