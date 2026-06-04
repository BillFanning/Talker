//! Profile schema data types (spec §72–§80, §80.1).
//!
//! These are pure, serde-serializable configuration structures — no behavior,
//! no runtime state (§5.7, §67). The interface/extraction/decoder enums are
//! **internally tagged** so every variant serializes as a TOML table (a unit
//! variant like `Stream` becomes `{ method = "Stream" }`); this keeps the TOML
//! shape uniform and avoids serializer ordering pitfalls.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::{ChannelKind, ChannelName, ProtocolId, StableConfigId};
use crate::decode::NmeaValidationMode;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, WrappingMode};
use crate::record::{FileRotationPolicy, OverwritePolicy, RecordingMode};
use crate::transport::udp::UdpMode;

/// One configured Channel (§72). Carries only configuration; runtime objects
/// (connections, message numbers, history) are never stored here (§69).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub id: Option<StableConfigId>,
    pub name: ChannelName,
    pub kind: ChannelKind,
    pub interface: InterfaceConfig,
    #[serde(default)]
    pub extraction: ExtractionConfig,
    #[serde(default)]
    pub decoder: DecoderConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub recording: RecordingConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Opt-in auto-reconnect after a fault (§9.1, §162); default disabled.
    #[serde(default)]
    pub reconnect: ReconnectPolicy,
}

/// Opt-in auto-reconnect policy (§9.1, §162). When `enabled`, a Channel that
/// faults while it was meant to be Running is automatically re-Started with
/// exponential backoff. Applies to Serial/UDP/TCP-Listener Channels, never TCP
/// Connection Channels (the listener does not dial out).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    #[serde(default = "default_max_backoff_ms")]
    pub max_backoff_ms: u64,
    #[serde(default = "default_backoff_multiplier")]
    pub multiplier: f64,
    /// `None` = retry indefinitely.
    #[serde(default)]
    pub max_attempts: Option<u32>,
}

fn default_initial_backoff_ms() -> u64 {
    1_000
}
fn default_max_backoff_ms() -> u64 {
    30_000
}
fn default_backoff_multiplier() -> f64 {
    2.0
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_backoff_ms: default_initial_backoff_ms(),
            max_backoff_ms: default_max_backoff_ms(),
            multiplier: default_backoff_multiplier(),
            max_attempts: None,
        }
    }
}

/// Interface configuration (§73). There is no persisted `TcpConnectionConfig` —
/// connection channels are runtime-only (§16.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InterfaceConfig {
    Serial(SerialConfig),
    Udp(UdpConfig),
    TcpListener(TcpListenerConfig),
}

/// Serial interface configuration (§74). Uses the listener-level §80.1 enums
/// (richer than what `serialport` supports); the runtime maps them and rejects
/// unsupported combinations at Start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SerialConfig {
    pub port: String,
    pub baud_rate: u32,
    #[serde(default)]
    pub data_bits: DataBits,
    #[serde(default)]
    pub parity: Parity,
    #[serde(default)]
    pub stop_bits: StopBits,
    #[serde(default)]
    pub flow_control: FlowControl,
    #[serde(default)]
    pub rts: Option<bool>,
    #[serde(default)]
    pub dtr: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Parity {
    #[default]
    None,
    Even,
    Odd,
    Mark,
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StopBits {
    #[default]
    One,
    OnePointFive,
    Two,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FlowControl {
    #[default]
    None,
    RtsCts,
}

/// UDP interface configuration (§75).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UdpConfig {
    pub bind_address: String,
    pub port: u16,
    pub mode: UdpMode,
    #[serde(default)]
    pub multicast_group: Option<String>,
    /// Multicast join interface — a local IPv4 address selecting the NIC on a
    /// multi-homed host (§75/§167). `None` = the OS default interface.
    #[serde(default)]
    pub multicast_interface: Option<String>,
    /// SO_RCVBUF in bytes (§167): the primary lever against kernel-dropped UDP
    /// (§101). `None` = OS default. Applied at bind; a change takes effect via the
    /// §13 apply-pending restart.
    #[serde(default)]
    pub recv_buffer_bytes: Option<usize>,
}

/// TCP listener interface configuration (§76).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TcpListenerConfig {
    pub bind_address: String,
    pub port: u16,
    #[serde(default)]
    pub max_connections: Option<u32>,
    /// SO_RCVBUF in bytes for accepted connections (§167); `None` = OS default.
    #[serde(default)]
    pub recv_buffer_bytes: Option<usize>,
}

/// Message-extraction configuration (§20). For a TCP Listener these settings
/// apply to its accepted connection channels (§16.2).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "method")]
pub enum ExtractionConfig {
    #[default]
    Stream,
    Delimiter {
        delimiter: Vec<u8>,
        include_delimiter: bool,
    },
    FixedLength {
        length: usize,
        sync_marker: Option<Vec<u8>>,
    },
    Protocol {
        protocol: ProtocolId,
    },
}

/// Decoder selection (§77). Explicit per Channel; no auto-detection (§30).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(tag = "decoder")]
pub enum DecoderConfig {
    #[default]
    None,
    Nmea0183 {
        validation_mode: NmeaValidationMode,
    },
}

/// Per-sink subsampling policy (§50.1): which Messages pass to a sink, to keep a
/// fast stream readable or a log compact. Applies to display views and
/// message-oriented recordings; raw byte data (`.dat`) is never subsampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "policy")]
pub enum Subsample {
    #[default]
    None,
    /// Count-based: pass one of every `n` Messages (`n >= 1`).
    EveryNth { n: u32 },
    /// Time-based: pass at most one Message per `millis` milliseconds.
    RateLimit { millis: u64 },
}

/// Display configuration: a set of views (§78).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DisplayConfig {
    #[serde(default)]
    pub views: Vec<DisplayViewConfig>,
}

/// One display view's configuration (§78). Visual-only fields (font, colors) do
/// not affect produced text (see `display::DisplayView`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayViewConfig {
    pub mode: DisplayMode,
    pub encoding: DisplayEncoding,
    pub character_rendering: CharacterRendering,
    #[serde(default)]
    pub font: Option<String>,
    #[serde(default)]
    pub foreground_color: Option<String>,
    #[serde(default)]
    pub background_color: Option<String>,
    pub wrapping: WrappingMode,
    #[serde(default)]
    pub metadata_visible: bool,
    /// Subsampling for this view's on-screen history (§50.1); default `None`.
    #[serde(default)]
    pub subsample: Subsample,
}

/// Recording configuration (§79).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct RecordingConfig {
    #[serde(default)]
    pub mode: RecordingMode,
    /// A single file when `rotation` is `None`; the output **directory** otherwise
    /// (§59), into which `<channel>_<period><ext>` files are written.
    #[serde(default)]
    pub destination: Option<PathBuf>,
    #[serde(default)]
    pub timestamp_enabled: bool,
    #[serde(default)]
    pub overwrite_policy: OverwritePolicy,
    /// Time-based file rotation (§59); `None` = single file (additive, §72.1).
    #[serde(default)]
    pub file_rotation: FileRotationPolicy,
    /// Subsampling (§50.1). For a `Raw` recording a non-`None` policy makes it a
    /// message-framed, decimated **`.ssdat`** data file rather than a byte-exact
    /// `.dat` (raw byte data is never subsampled, §53). Default `None`.
    #[serde(default)]
    pub subsample: Subsample,
    /// Disk-space guard for long-running recordings (§56.2, §168); `None` = off.
    #[serde(default)]
    pub disk_guard: Option<DiskGuard>,
}

/// Disk-space guard for a recording (§56.2, §168). When free space on the
/// destination filesystem falls below `min_free`, Listener warns and, if
/// `on_low` is `StopRecording`, finalizes the recording cleanly and stops it while
/// reception continues. Free space is polled periodically, not per write.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiskGuard {
    pub min_free: DiskThreshold,
    pub on_low: LowDiskAction,
}

/// A low-disk threshold (§168): absolute bytes or a percentage of the filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DiskThreshold {
    Bytes { bytes: u64 },
    Percent { percent: u8 },
}

/// What to do when free disk space is low (§168).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LowDiskAction {
    /// Warn only; recording continues.
    #[default]
    Warn,
    /// Finalize and stop the recording cleanly; reception continues (§96).
    StopRecording,
}

/// Retention limits (§80). At least one applicable limit must be set — an
/// all-`None` config is rejected by validation (§71) and bounded by the runtime
/// backstop regardless (§80, retention module).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RetentionConfig {
    #[serde(default)]
    pub message_limit: Option<usize>,
    #[serde(default)]
    pub byte_limit: Option<usize>,
    #[serde(default)]
    pub event_limit: Option<usize>,
    #[serde(default)]
    pub warning_limit: Option<usize>,
    #[serde(default)]
    pub error_limit: Option<usize>,
}

impl RetentionConfig {
    /// True when no limit is set (would leave retention unbounded).
    pub fn is_unbounded(&self) -> bool {
        self.message_limit.is_none()
            && self.byte_limit.is_none()
            && self.event_limit.is_none()
            && self.warning_limit.is_none()
            && self.error_limit.is_none()
    }

    /// A retention config bounded by a message count (a sane template default).
    pub fn with_message_limit(limit: usize) -> Self {
        Self {
            message_limit: Some(limit),
            ..Self::default()
        }
    }
}

/// Profile-level defaults applied to channels that do not override them (§80.1).
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DefaultConfig {
    #[serde(default)]
    pub display: Option<DisplayConfig>,
    #[serde(default)]
    pub retention: Option<RetentionConfig>,
}
