//! Channel, display-view, and configuration identifiers (spec §129, §80.1).
//!
//! `ChannelId` and `DisplayViewId` are runtime objects minted while Listener
//! runs (§97.1: the runtime mints `ChannelId`s, including one per accepted TCP
//! connection). `StableConfigId` is the persisted identity stored in a profile
//! (§72, `ChannelConfig::id`).

use std::fmt;

use uuid::Uuid;

/// Unique internal identifier for a Channel (§6, §129). Runtime-minted; never
/// persisted in a profile (§69).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(Uuid);

impl ChannelId {
    /// Mint a fresh, random channel id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ChannelId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// User-visible Channel name (§6). User configurable and need not be unique.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelName(String);

impl ChannelName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ChannelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Runtime identity of a single Display View (§11). A Channel may own several;
/// pausing one does not pause the others. Runtime-only — never persisted (§69).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DisplayViewId(Uuid);

impl DisplayViewId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DisplayViewId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity for a configured Channel, persisted in a profile so a saved
/// workspace round-trips to the same logical channel (§72, §80.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StableConfigId(Uuid);

impl StableConfigId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for StableConfigId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_ids_are_unique() {
        assert_ne!(ChannelId::new(), ChannelId::new());
    }

    #[test]
    fn channel_name_round_trips() {
        let name = ChannelName::new("GPS Receiver");
        assert_eq!(name.as_str(), "GPS Receiver");
        assert_eq!(name.to_string(), "GPS Receiver");
    }
}
