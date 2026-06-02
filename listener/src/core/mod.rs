//! Common listener types, message model, metadata model, IDs, states, and shared errors.
//!
//! This is `listener-core` (spec §128): the pure data model shared by every
//! other module. It owns common types but avoids I/O, UI, and async-runtime
//! coupling — transports, the runtime, and the presentation layers depend on
//! these types, not the reverse.

pub mod command;
pub mod error;
pub mod ids;
pub mod message;
pub mod metadata;
pub mod state;

pub use command::{RuntimeCommand, RuntimeEvent};
pub use error::{DecodeError, ErrorCategory, RecordError};
pub use ids::{ChannelId, ChannelName, DisplayViewId, StableConfigId};
pub use message::{ChunkTime, Message, MessageBytes, MessageMetadata, MessageTimestamp};
pub use metadata::{
    IntegrityMetadata, IntegrityScope, IntegrityStatus, ProtocolId, ProtocolMetadata,
};
pub use state::{ChannelKind, ChannelState, DisplayState, RecordingState};
