//! Profile schema data types (spec §72–§80, §80.1).
//!
//! These are pure, serde-serializable configuration structures — no behavior,
//! no runtime state (§5.7, §67). The interface enums are
//! **internally tagged** so every variant serializes as a TOML table (a unit
//! variant like `Stream` becomes `{ method = "Stream" }`); this keeps the TOML
//! shape uniform and avoids serializer ordering pitfalls.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::{ChannelKind, ChannelName, StableConfigId, TimestampConfig};
use crate::diagnostics::DiagnosticSeverity;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, WrappingMode};
use crate::record::{FileRotationPolicy, OverwritePolicy};
use crate::transport::udp::UdpMode;

/// One configured Channel (§72). Carries only configuration; runtime objects
/// (live connections, the byte stream, diagnostics) are never stored here (§69).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    #[serde(default)]
    pub id: Option<StableConfigId>,
    pub name: ChannelName,
    pub kind: ChannelKind,
    pub interface: InterfaceConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    /// Raw recording (§53) — taps the verbatim byte stream. Independent of display
    /// recording (ADR-013).
    #[serde(default)]
    pub raw_recording: RawRecordingConfig,
    /// Display recording (§54) — records the rendered view (`.disp`). Independent of
    /// raw recording (ADR-013).
    #[serde(default)]
    pub display_recording: DisplayRecordingConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Opt-in auto-reconnect after a fault (§9.1, §162); default disabled.
    #[serde(default)]
    pub reconnect: ReconnectPolicy,
    /// Per-Channel Match Rules (§50.2, §165); default empty. Presentation/control
    /// only — a rule never modifies the stream bytes, recordings, or metadata.
    #[serde(default)]
    pub match_rules: Vec<MatchRule>,
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
/// Serde default for opt-out booleans (e.g. a Match Rule is enabled unless the
/// profile says otherwise, §50.2).
fn default_true() -> bool {
    true
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
    /// Software flow control (XON/XOFF).
    XonXoff,
    /// Hardware flow control (RTS/CTS).
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
    /// Monospace point size for the viewer. `None` = the GUI default. Additive
    /// (`#[serde(default)]`), so profiles written before this field round-trip (§78).
    #[serde(default)]
    pub font_size: Option<f32>,
    #[serde(default)]
    pub foreground_color: Option<String>,
    #[serde(default)]
    pub background_color: Option<String>,
    pub wrapping: WrappingMode,
    /// Byte grouping/spacing for the Hex view (§45). Ignored by the other display
    /// modes. `#[serde(default)]` so profiles written before this field round-trip.
    #[serde(default)]
    pub hex_grouping: HexGrouping,
}

/// Hex-view byte grouping (§45): how many bytes share a group (separated by
/// spaces) and how many groups fill a line. `groups_per_line == 0` means "fit to
/// the display width" rather than a fixed count.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HexGrouping {
    pub bytes_per_group: u8,
    pub groups_per_line: u8,
}

impl Default for HexGrouping {
    /// The conventional hexdump default: one byte per group, fit to width (§45).
    fn default() -> Self {
        Self {
            bytes_per_group: 1,
            groups_per_line: 0,
        }
    }
}

/// Raw recording configuration (§53, §79). Raw recording taps the **verbatim byte
/// stream** — exactly as received, before any rendering — and is configured
/// independently of Display recording (they branch at different points in the
/// pipeline and were always separate; the v2.0 strip merged them in config only).
/// Listener ADR-013.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RawRecordingConfig {
    /// Whether to begin Raw recording automatically when the Channel starts. The
    /// live Record toggle (ADR-012) can begin/stop it at runtime regardless, as long
    /// as a `destination` is set (the destination arms the toggle).
    #[serde(default)]
    pub enabled: bool,
    /// A single `.raw` file when `rotation` is `None`; the output **directory**
    /// otherwise (§59), into which `<channel>_<period>.raw` files are written.
    #[serde(default)]
    pub destination: Option<PathBuf>,
    #[serde(default)]
    pub timestamp_enabled: bool,
    #[serde(default)]
    pub overwrite_policy: OverwritePolicy,
    /// Time-based file rotation (§59); `None` = single file (additive, §72.1).
    #[serde(default)]
    pub file_rotation: FileRotationPolicy,
    /// Disk-space guard for long-running recordings (§56.2, §168); `None` = off.
    #[serde(default)]
    pub disk_guard: Option<DiskGuard>,
}

impl Default for RawRecordingConfig {
    /// Defaults to **Append** on-exists (not the global `Refuse`): re-recording to the
    /// same `.raw` file extends it rather than failing, which is the friendly default
    /// for a capture tool. Everything else is off/none.
    fn default() -> Self {
        Self {
            enabled: false,
            destination: None,
            timestamp_enabled: false,
            overwrite_policy: OverwritePolicy::AppendIfExists,
            file_rotation: FileRotationPolicy::None,
            disk_guard: None,
        }
    }
}

/// Display recording configuration (§54, §79). Display recording records the
/// **rendered view** output (`.disp`) — a separate pipeline tap from Raw recording
/// (ADR-013), with its own destination and options.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct DisplayRecordingConfig {
    /// Whether to begin Display recording automatically when the Channel starts.
    #[serde(default)]
    pub enabled: bool,
    /// A single `.disp` file when `rotation` is `None`; the output **directory**
    /// otherwise (§59).
    #[serde(default)]
    pub destination: Option<PathBuf>,
    #[serde(default)]
    pub overwrite_policy: OverwritePolicy,
    /// Time-based file rotation (§59); `None` = single file.
    #[serde(default)]
    pub file_rotation: FileRotationPolicy,
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

/// A per-Channel Match Rule (§50.2, §165): a predicate over received data that
/// fires one or more presentation/control Actions on a match. Rules are
/// **presentation/control only** — they never modify the stream bytes, recordings,
/// or metadata (§40, §103, §116). Persisted in profiles; identified at runtime by
/// a minted [`MatchRuleId`](crate::core::MatchRuleId), in config by `name`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchRule {
    pub name: String,
    pub condition: MatchCondition,
    #[serde(default)]
    pub actions: Vec<MatchAction>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// What a Match Rule tests (§50.2). One condition per rule — compound
/// AND/OR/sequence logic is deferred (Appendix A). Internally tagged so every
/// variant is a uniform TOML table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MatchCondition {
    /// A byte pattern scanned across the received stream (§50.2), matching across
    /// receive-chunk boundaries.
    BytePattern { pattern: Vec<u8> },
    /// No data received for `timeout_ms` (timer-based, via the activity monitor
    /// §91.1): fires once when quiet and re-arms when data resumes. (The spec
    /// shows a `Duration`; config carries milliseconds, like `ReconnectPolicy`.)
    Idle { timeout_ms: u64 },
}

/// What a matched rule does (§50.2). Presentation/control only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum MatchAction {
    /// Begin or stop recording **from the match forward** — no pre-match backfill
    /// (§158). Requires the Channel to have a recording destination configured.
    Record {
        target: RecordTarget,
        control: RecordControl,
    },
    /// Drop a correlation marker into the display, the Display Recording (`.disp`),
    /// and a tagged event (§137) — **never** into the raw `.raw` stream (§5.6/§49).
    ///
    /// When `timestamp` is `Some`, the marked byte pattern also gets an inline local
    /// arrival timestamp spliced into the rendered text (display + `.disp`) before or
    /// after the match (§50.2). A bare `Mark` (no timestamp) keeps the `‹MARK …›`
    /// marker-line behaviour.
    Mark {
        #[serde(default)]
        timestamp: Option<MarkTimestamp>,
    },
    /// Raise a diagnostic event/warning of the given severity (§92–§94).
    Notify { severity: DiagnosticSeverity },
    /// Freeze a Display View by index (into `display.views`), or all views when
    /// `None` (§50). Reception and recording continue. (The spec models the target
    /// as a runtime `DisplayViewId`; config uses a stable view index.)
    PauseDisplay {
        #[serde(default)]
        view: Option<usize>,
    },
}

/// The inline timestamp a `Mark` action splices next to a matched byte pattern
/// (§50.2). The time is the matched chunk's **arrival** time, formatted in local
/// time per `format`, and inserted `position` (before/after) the match in the
/// rendered display and Display Recording (`.disp`) — never in `.raw`.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MarkTimestamp {
    #[serde(default)]
    pub position: MarkPosition,
    #[serde(default)]
    pub format: TimestampConfig,
    /// Text appended immediately **after** the formatted timestamp (e.g. a space
    /// or `", "`) to separate it from the adjacent data, in both positions:
    /// `Before` renders `[ts][sep]match…`, `After` renders `…match[ts][sep]`.
    /// Empty = nothing appended. Additive (`#[serde(default)]`), so profiles
    /// written before this field round-trip (§72.1).
    #[serde(default)]
    pub separator: String,
}

/// Where a `Mark` timestamp is spliced relative to the matched bytes (§50.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MarkPosition {
    /// Immediately before the first byte of the match (e.g. `[17:42:03]$GPGGA…`).
    #[default]
    Before,
    /// Immediately after the last byte of the match.
    After,
}

/// Which recording(s) a `Record` action controls (§50.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordTarget {
    Raw,
    Display,
    Both,
}

/// Whether a `Record` action begins or stops recording (§50.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordControl {
    Begin,
    Stop,
}

/// Retention limits (§80). At least one applicable limit must be set — an
/// all-`None` config is rejected by validation (§71) and bounded by the runtime
/// backstop regardless (§80, retention module).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RetentionConfig {
    /// Stream-scrollback byte limit (replaces the v1 message_limit).
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
        self.byte_limit.is_none()
            && self.event_limit.is_none()
            && self.warning_limit.is_none()
            && self.error_limit.is_none()
    }

    /// A retention config bounded by a byte count (a sane template default).
    pub fn with_byte_limit(limit: usize) -> Self {
        Self {
            byte_limit: Some(limit),
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
