# Listener Specification v2.2

Status: Draft (v2.0 — stream-only architecture; the Message infrastructure is removed)
Audience: human reviewers, Rust implementers, and code-generation agents
Primary implementation language: Rust
Primary editor workflow: VS Code + rust-analyzer

Revision v2.2 (NMEA ZDA Mark annotations and exact splice geometry):

- **§50.2 Mark styles (ADR-025).** A timestamped Mark may retain compact local
  time or emit an inline checksum-bearing NMEA ZDA sentence. ZDA time/date fields
  are UTC, zone fields carry the local offset, milliseconds are optional, and
  custom talker IDs longer than two characters are supported under a bounded
  framing-safe policy.
- `Before` anchors on the first match byte and `After` on the final match byte.
  Separators remain verbatim and may contain CR/LF; Raw, Rendered, and Hex renderers
  reset their line/column continuation state around those controls. `.raw` remains
  byte-exact.
- Listener again depends on `nmea0183`, solely to construct presentation text. It
  still performs no protocol decoding or interpretation of received bytes.
- `MarkTimestamp.style` is additive and defaults to `Plain`; profile
  `schema_version` remains 3.

Revision v2.1 (scope amendments — one display view per channel; TCP connection-channel
visibility deferred; a §155 correction):

- **§48 one Display View per Channel (ADR-023).** v2.1 narrows §48 from "multiple
  simultaneous Display Views" to exactly **one logical Display View per Channel**,
  with Raw / Rendered / Hex as that view's switchable *modes*. This documents shipped
  reality (pause gating and the GUI have only ever used the first view) and matches
  talker's one-Output-pane model. The profile schema's `views` list is unchanged
  (additive forward-compatibility); entries beyond the first are ignored. Multiple
  simultaneous views move to Appendix A.
- **§16 TCP Connection-Channel visibility is deferred (ADR-024; parked by user
  decision 2026-07-11).** Accepted connections run full pipelines and emit
  connect/disconnect lifecycle events, but are not individually inspectable (no
  per-connection snapshot, stream view, recording, or match rules), and
  `recv_buffer_bytes` is not yet applied to accepted sockets. §16.2's independence
  requirements remain the architecture; their user-facing surfacing moves to
  Appendix A until a real TCP-inspection need promotes it.
- **§155 correction** — the BytePattern acceptance criterion said matches are
  "highlighted in the stream"; on-screen highlighting was removed in v2.0
  (ADR-015). The criterion now names the shipped observables: the rule-firing
  log and inline `Mark` splices.

Revision v2.0.7 (inline Mark timestamps; remove two abandoned timestamp pieces):

The `Mark` match action may now carry an optional **inline timestamp** (§50.2): the
matched byte pattern's local **arrival** time is spliced into the rendered display and
the Display Recording (`.disp`) — configurably **before** or **after** the match —
never into the byte-exact `.raw` stream (§53). The timestamp format mirrors talker's
toggleable `TimestampConfig` (time-of-day always shown; date / milliseconds / local UTC
offset optional), formatted in local time. It is rendered by splicing the timestamp
string at the match's byte offset in every view mode (Raw/Rendered/Hex) — a text
insertion, not on-screen byte-range styling (contrast the removed `Highlight`, ADR-015).
A bare `Mark` (no timestamp) keeps its `‹MARK …›` marker-line behaviour.

Two abandoned, never-completed timestamp pieces are removed so this is the **only**
timestamping mechanism: (a) the per-chunk Display-Recording timestamp
(`DisplayRecordingConfig.timestamp_enabled` and its GUI checkbox) — it duplicated
nothing useful and was never surfaced; and (b) the spec-only
`TimestampDisplay`/`TimestampSource`/`TimestampResolution` types (§26/§57/§72), which had
no implementation. The byte-exact **Raw Recording timestamp sidecar** (`.raw.idx`, §57)
is **kept** — it stays out of the `.raw` bytes — with a TODO to expose it in the UI. No
`schema_version` change (the removed display field is an additive-safe drop; dev-only
profiles). See the new ADR.

Revision v2.0.6 (drop the Highlight match action):

The `Highlight` match action and its `HighlightStyle` type are removed (§50.2/§165/§72).
`Highlight` was never rendered — precise on-screen byte-range styling across all view
modes was disproportionate to listener's simple goal; the `Mark` action already serves
correlation into the display recording. The remaining Find & Triggers actions
(`Record`/`Mark`/`Notify`/`PauseDisplay`) and conditions (`BytePattern`/`Idle`) are
unchanged. No `schema_version` change (variant removal; dev-only profiles). See ADR-015.

Revision v2.0.5 (diagnostics retention across a restart):

New §89.1: a Channel's retained diagnostics (§92–§94) persist across that Channel's
own stop→start within a session, so the log spans a restart instead of starting blank;
a start that faults (e.g. a bind conflict) is retained as an Error and survives the
next start. Bounded by the §88 limits; runtime-only (not persisted to disk); the stream
scrollback is not carried across a restart (§8.5). See listener ADR-006. No schema
change.

Also reconciles the docs with the code (doc-catch-up, no behavior change): §137 gains
`RecordingStarted` (present since ADR-012/-013; it lets observers clear a prior
recording fault); §99 describes diagnostics retention as the per-severity count-bounded
`DiagnosticLog` actually shipped, not the abandoned single drop-oldest-low-priority
queue.

Revision v2.0.4 (recording-destination uniqueness):

Two recordings may no longer write the same file (§121, ADR-014). §6 makes Channel
Names **unique** (was "need not be unique"); §71 validation rejects a duplicate name;
§55 fails enabling (as a *recording* fault, Channel stays Running) if an OS advisory
lock on the destination cannot be taken; §121 states the two-layer rule (unique names +
advisory lock). No schema change — names were already required filesystem-safe.

Revision v2.0.3 (recording-config reconciliation):

§52 and §72 are reconciled to the §79 independent Raw/Display recording config
(v2.0.2, ADR-013): §52 no longer defines a `RecordingMode` enum, and §72's
`ChannelConfig` carries `raw_recording` + `display_recording` instead of a single
`recording` field. No behavior change — these sections were stale after the v2.0.2
edit only updated §79.

Revision v2.0.2 (independent Raw/Display recording config):

§79 replaces the single `RecordingConfig`/`RecordingMode` with independent
`RawRecordingConfig` and `DisplayRecordingConfig` — Raw and Display recording tap the
pipeline separately and are now configured separately (each with its own destination),
so a Channel may run both at once. `schema_version` bumps to 3 (clean break; v1/v2
profiles refused). See listener ADR-013.

Revision v2.0.1 (command-surface reconciliation):

§136 no longer defines a `RuntimeCommand` enum. The command surface (UI → runtime)
is the `Listener`'s async method API directly; the GUI's own `UiCommand` is the
presentation-layer transport the driver maps onto those calls. See listener ADR-012.
`RuntimeEvent` (§137) is unchanged.

Revision v2.0 (architecture pivot — **stream-only**):

`Listener` is now a pure **stream** acquisition / inspection / recording tool.
Received bytes are a single verbatim stream that is displayed (Raw / Rendered / Hex)
and recorded. The entire Message infrastructure is removed — there is no Message
Mode, Message Extraction, Message Numbering, decoders, NMEA decoding, integrity
metadata, message-framed recording, or message-keyed display.

Removed (the superseded sections are kept as numbered stubs so existing §N
cross-references stay valid): §19 Message Mode; §20–23 Extraction; §24 Message
Number; §28 Integrity Metadata; §29–39 Decoders + all NMEA sections; §49 Metadata
Display; §50.1 Subsampling; §77 Decoder Config; §83 NMEA Serial Template; §131–135
Message types; §139/§140 Extractor/Decoder traits; §105/§107 Extractor/Decoder
ownership; and the message-framed recording path.

Rebased onto the stream:
- §17–18 — the input model is **stream only**; no per-channel Stream/Message switch.
- §40–46 — the display renders the verbatim stream (Raw/Rendered/Hex + character
  rendering); §41 has a single source (the stream).
- §50.2 / §165 — **Find & Triggers**: conditions are `BytePattern` (cross-chunk
  stream scan) and `Idle`; actions are
  `Record { Begin | Stop, target: Raw | Display | Both }`, `Notify`, and `Mark` (a
  time/offset marker into the display and the display recording). No decoded-field or
  message-size conditions; `Mark` anchors on a **byte offset**, not a Message Number.
  (An on-screen `Highlight` styling action was considered and dropped — see ADR-015.)
- §51–59 / §79 / §142 — recording is **Raw** (byte-exact, extension **`.raw`**) and
  **Display** (the rendered output of the active view mode, extension **`.disp`**),
  each with optional timestamps and time-based rotation. `.ssdat` and subsampling are
  removed; `.raw` replaces the former `.dat`.

`schema_version` bumps (a breaking config change: the extraction, decoder, subsample,
and message-recording fields are gone). `nmea0183` remains a workspace crate.
Listener v2.2 reacquires it only to construct presentation-only ZDA Mark annotations;
the receive path still performs no decoding.

Revision v1.2 (feature expansion — troubleshooting & long-run logging). _Parts of
this note are superseded by v2.0 above — the supersessions are flagged inline._
- §59 — **time-based file rotation** (Hourly/Daily); generated filenames
  `<channel>_<start-time>`. _v2.0: extensions are now **`.raw`** (raw data) and
  **`.disp`** (display recording), plus **`.log`** (diagnostic log only); the former
  `.dat`/`.ssdat` are gone (§50.1)._ Filesystem-safe channel-name constraints (§71).
  Size-based rotation stays deferred.
- §50.1 — sink subsampling. _Removed in v2.0 (it was a Message-oriented filter)._
- §50.2 — **Match Rules & Triggers**: a predicate fires actions (highlight, begin/stop
  recording, mark, notify, pause). _v2.0: conditions are byte-pattern / idle only; the
  decoded-field and message-size conditions are removed (§50.2)._
- §14.3/§14.4 — full **serial control-line** monitor + control (RTS/DTR set and
  live-toggle; CTS/DSR/DCD/RI live display); port enumeration + hot-plug; RS-422/485
  phased.
- §75/§76 — **network live adjustment**: SO_RCVBUF, multicast join/leave + interface.
- §78 — display extras: per-view **source**, **timestamp format**, **hex grouping**, and
  per-message **annotation toggles** (number + timestamp) replacing `metadata_visible`.
  _v2.0.7: the per-view timestamp-format display extra and its `TimestampDisplay`/
  `TimestampSource`/`TimestampResolution` types are removed (never implemented); the only
  display timestamp is the per-match `Mark` timestamp (§50.2)._
- §9.1 opt-in **auto-reconnect**; §91.1 **liveness / activity monitor**; §56.2
  **disk-space guard** for recording.
- §136/§137 — command/event vocabulary expanded (incl. a dedicated `ReceptionStalled`
  event resolving the ADR-007 overload); both enums now `#[non_exhaustive]`.
Config changes are additive (`#[serde(default)]`), so `schema_version` is unchanged;
`metadata_visible` is retired (old profiles still load).

Revision v1.1.1:
- Spec relocated to the crate's own `docs/` directory; document-location paths corrected.
- §127/§128 — resolved OQ-L1: `Listener` ships as a **single `listener` crate** with
  the proposed `listener-*` units realized as `src/` modules (boundaries unchanged);
  corrected the layout that nested `nmea0183` (it is a top-level sibling crate).
No runtime, data-model, or schema changes from v1.1 — the packaging decision does not
alter the normative module boundaries or behavior.

---

## 0. Purpose of This Document

This document defines the functional requirements, architecture, runtime model, and recommended Rust implementation structure for `Listener`.

`Listener` is a cross-platform utility for receiving byte-oriented data from serial and network sources, displaying that verbatim byte stream in multiple representations, and optionally recording it to files.

This document is intended to be placed at:

```text
listener/docs/listener_specification.md
```

Codex and other code-generation tools should treat this specification as authoritative unless the user explicitly revises it.

---

# Part I — Product Definition

## 1. Product Overview

`Listener` is a streamed communication data acquisition, inspection, display, and recording application. It treats every source as a verbatim byte stream — it does not parse, frame, or decode that stream into messages.

Primary use cases:

- Inspect data arriving from serial ports.
- Inspect UDP unicast, broadcast, and multicast data.
- Listen for TCP clients and inspect their transmitted data.
- Display the received byte stream in Raw, Rendered, and Hex views.
- Find and highlight byte patterns in the stream, and trigger actions on them.
- Record the original received bytes or the rendered display output.

`Listener` is a passive receive-side tool. It observes, displays, and records. It is not a packet injection tool, control system, SCADA system, or hard real-time system.

## 2. Supported Platforms

Listener shall run natively on:

- Windows
- Linux
- macOS

Platform-specific behavior shall be isolated to transport, file-system, and UI integration layers where practical.

## 3. Interfaces

Listener shall eventually support:

- CLI operation
- GUI operation

Both interfaces shall be thin presentation layers over shared core/runtime logic.

The UI shall not own transport handles, recording handles, or channel pipeline state.

---

# Part II — Core Terms and Conceptual Model

## 4. Definitions

### 4.1 Channel

A Channel is a configured source of received data or connection events.

Channel types:

- Serial Channel
- UDP Channel
- TCP Listener Channel
- TCP Connection Channel

For user-created Serial, UDP, and TCP Listener Channels, the `+ Add` template chooses
the transport kind. Configure Connection edits that kind's parameters but does not
replace it with another transport. TCP Connection Channels remain runtime-created
children of a TCP Listener Channel.

### 4.2 Byte Stream

A Byte Stream is an ordered sequence of received bytes.

Serial and TCP Connection Channels produce Byte Streams.

### 4.3 Stream

The received data is a single verbatim **byte stream** — the bytes as the transport read them, in arrival order. `Listener` does not segment the stream into records. (UDP datagrams arrive with their boundaries preserved, §15, but those boundaries are a reception/recording detail, not a unit the stream is reframed around.)

### 4.4 Message Extraction

_Removed in v2.0. There is no extraction (§20)._

### 4.5 Decoder

_Removed in v2.0. There is no decoder (§29)._

### 4.6 Metadata

_Removed in v2.0. The only reception facts kept are the running byte count (§25) and the per-chunk arrival time (§26); there is no per-Message metadata._

### 4.7 Payload Metadata

_Removed in v2.0 (see §4.6)._

### 4.8 Protocol Metadata

_Removed in v2.0 (see §4.5)._

### 4.9 Integrity Metadata

_Removed in v2.0 (see §4.5)._

### 4.10 Display View

A Display View is a presentation of Channel data.

Multiple Display Views may reference the same Channel.

### 4.11 Recording

Recording is live persistent output written while data is being received.

Recording is distinct from Export.

### 4.12 Export

Export is an on-demand operation performed on currently retained runtime information.

Export is distinct from Recording.

### 4.13 Profile

A Profile is a persisted Listener workspace configuration.

Profiles store configuration only.

### 4.14 Runtime Object

A Runtime Object exists only while Listener is running and is not persisted in Profiles.

Examples:

- TCP Connection Channels
- Display History (stream scrollback)
- Active file handles
- Runtime warnings/errors

---

# Part III — Architectural Principles

## 5. Core Principles

### 5.1 Byte Stream First

Received bytes are the authoritative source. Display, recording, find/triggers, and export are all derived from the verbatim byte stream — nothing reframes or reinterprets it.

### 5.2 No Reframing

The stream is never segmented into messages or records. There is no extraction stage and no message boundary (§17–18).

### 5.3 No Decoding

`Listener` does not interpret protocol meaning. NMEA and any other protocol are carried as ordinary bytes (§29).

### 5.4 Display Is Presentation Only

Display rendering shall not alter:

- Received bytes
- Raw recordings

### 5.5 Recording Fidelity

Raw recording is byte-exact (§53). Display recording reflects exactly what the chosen view renders (§54). Neither alters the received bytes.

### 5.6 Recording Preserves Original Data

Raw Recording is the authoritative persistent representation of the received data
it contains. It is byte-exact and contiguous up to a known end, but is not
guaranteed complete under sustained overload — see fault behavior (§56.1).

Raw Recording shall preserve:

- Original byte values
- Original byte ordering
- Original chunk / datagram ordering

### 5.7 Configuration and Runtime Separation

Profiles store configuration, not runtime state.

### 5.8 Failure Isolation

Subsystem failures shall remain isolated whenever practical.

A Recording failure shall not stop Reception if Reception can safely continue.

### 5.9 Acquisition Priority

Reception is higher priority than display completeness.

When pressure occurs, Listener should preserve acquisition, degrade display if necessary, and report loss.

### 5.10 Explicit User Control

Potentially disruptive actions shall require explicit user action whenever practical.

Examples:

- Starting Channels
- Applying restart-required configuration changes
- Overwriting files
- Enabling recording

---

# Part IV — Channel Model

## 6. Channel Identity

Each Channel shall have:

- A unique internal Channel Identifier
- A user-visible Channel Name
- A Channel Kind
- Configuration
- Runtime state

Channel Names are user configurable and shall be **unique** within a workspace
(§71, ADR-014): the name appears in generated recording filenames (§59), so two
Channels sharing a name could collide on a rotating recording destination.
Uniqueness is enforced at add-channel, at rename, and on profile load.

## 7. Channel Kinds

Supported Channel Kinds:

```rust
enum ChannelKind {
    Serial,
    Udp,
    TcpListener,
    TcpConnection,
}
```

## 8. Channel States

Supported Channel states:

```rust
enum ChannelState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Faulted,
}
```

### 8.1 Stopped

The Channel is inactive.

- Interface closed
- No reception
- No recording

### 8.2 Starting

The Channel is attempting to become operational.

Examples:

- Opening serial port
- Binding UDP socket
- Creating TCP listener

### 8.3 Running

The Channel is operational.

- Interface open
- Reception may occur
- Recording may occur if enabled

### 8.4 Stopping

The Channel is shutting down.

Examples:

- Closing serial port
- Closing socket
- Flushing recording buffers

### 8.5 Faulted

The Channel encountered an unrecoverable error that prevents normal operation.

A Faulted Channel may be returned to Stopped by user action.

## 9. State Transitions

Permitted transitions:

```text
Stopped → Starting → Running
Running → Stopping → Stopped
Starting → Faulted
Running → Faulted
Faulted → Stopped
```

Forbidden direct transitions:

```text
Stopped → Running
Running → Starting
Faulted → Running
```

### 9.1 Auto-Reconnect (opt-in)

By default a Faulted Channel stays Faulted until the user acts (§8.5). A Channel
may opt in to **auto-reconnect**: after a fault that ended a Channel which was
meant to be Running, the runtime automatically re-attempts Start with backoff
instead of remaining Faulted. This adds one transition, taken **only** when
auto-reconnect is enabled:

```text
Faulted → Starting   (auto-reconnect only, after a backoff delay)
```

Auto-reconnect is configured per Channel (`ReconnectPolicy`, §80.1) and is
**disabled by default**, preserving the manual-recovery semantics above. It
applies to Serial (covering USB hot-plug, §14.4), UDP, and TCP Listener Channels.
It does **not** apply to TCP Connection Channels: the listener accepts connections
and does not dial out (TCP client mode is deferred, Appendix A), so a dropped
client simply ends. Each attempt emits a reconnect event (§137); after
`max_attempts` (if set) the Channel remains Faulted.

## 10. Start and Stop

Start and Stop are Channel-level commands.

### 10.1 Start

Start shall:

- Open the underlying communication interface.
- Begin receiving data.
- Reset the byte counter (§25) and begin recording if configured (§79).

### 10.2 Stop

Stop shall:

- Close the underlying communication interface.
- Terminate data reception.
- Attempt to flush accepted recording data.

## 11. Display Pause and Resume

Pause and Resume are display-level commands, not Channel-level commands, and they
apply per Display View (a Channel may have several — §48), identified at runtime
by a `DisplayViewId`. Pausing one view shall not pause the others. Pause state is
runtime-only and is never persisted in a Profile (§69).

Display states:

```rust
enum DisplayState {
    Active,
    Paused,
}
```

Display Pause shall not affect:

- Interface state
- Reception
- Recording
- The byte counter / liveness

## 12. Recording State

Recording state is independent of Channel state.

```rust
enum RecordingState {
    Disabled,
    Enabled,
    Faulted,
}
```

A Channel may be Running while Recording is Faulted.

## 13. Pending Configuration Changes

Some configuration changes require interface reinitialization.

Examples:

- Serial baud rate
- Serial parity
- Serial stop bits
- Serial flow control
- UDP bind port
- TCP listener port

When such a change is made while a Channel is Running:

- The change shall be accepted into pending configuration.
- The active interface shall continue using current settings.
- The Channel shall indicate pending changes.
- The user shall explicitly apply pending changes.

Applying pending changes shall perform one coordinated restart:

```text
Stop Channel
Apply Configuration
Start Channel
```

The byte counter (§25) resets only because the interface was stopped and started.

Restarting a TCP Listener terminates all of its TCP Connection Channels (they are
runtime-only — §16.3). A `TcpClientDisconnected` event (§137) shall be emitted for
each terminated connection. Nothing is restored on restart; new connections are
accepted fresh.

---

# Part V — Transport Requirements

## 14. Serial Channels

### 14.1 Configurable Serial Parameters

Serial Channels shall support:

- Port Name
- Baud Rate
- Data Bits
- Parity
- Stop Bits
- Flow Control

Baud Rate shall support both:

- Standard selectable values
- User-entered custom values

Standard values should include:

```text
110, 300, 1200, 4800, 9600, 19200, 38400,
57600, 115200, 921600
```

### 14.2 Serial Control Signals

When supported by the operating system and hardware, Listener shall display:

- CTS
- RTS
- DSR
- DTR
- DCD
- RI

Listener shall permit manual control of:

- RTS
- DTR

CTS, DSR, DCD, and RI are normally input/status lines relative to Listener and are not directly controlled by Listener.

Manual RTS/DTR changes shall not affect:

- Reception
- Recording

### 14.3 Live Control-Line State and Control

Control-line handling is a first-class troubleshooting surface, not just an
open-time setting. While a Serial Channel is Running:

- **Output lines RTS and DTR** are settable at open (§74 `rts`/`dtr`) **and
  live-toggleable** via runtime commands (`SetRts`/`SetDtr`, §136).
- **Input lines CTS, DSR, DCD, RI** are **live-monitored**; a change is reflected
  in Channel status/snapshot and signalled by `ControlLinesChanged` (§137).

The full state is reported as a unit:

```rust
struct SerialControlLines {
    rts: bool, dtr: bool,                        // outputs (Listener drives)
    cts: bool, dsr: bool, dcd: bool, ri: bool,   // inputs (device drives)
}
```

Control-line polling is bounded and shall not interfere with reception (§100).

### 14.4 Port Discovery and Hot-Plug

Listener shall **enumerate available serial ports** for selection (a bounded
blocking operation, §97.1) and shall also accept a **manually entered** port name
for uncommon or virtual ports it did not enumerate. Ports may appear and disappear
at runtime (USB-serial adapters): a port that vanishes faults its Channel (§94);
**auto-reconnect** (§9.1), if enabled, reopens it when it returns.

### 14.5 Electrical Standards

RS-232 is supported first; RS-422 and RS-485 follow. RS-485 is half-duplex and may
require RTS-driven transmit-direction control; Listener is receive-side, so this is
minimal, but the RTS control of §14.3 accommodates adapters that share a direction
line.

## 15. UDP Channels

UDP Channels shall support:

- Unicast
- Broadcast
- Multicast

A UDP Channel shall receive complete UDP datagrams.

UDP datagram boundaries shall be preserved (as a reception/recording detail, §53); the payload is otherwise carried as ordinary stream bytes.

## 16. TCP Listener and TCP Connection Channels

### 16.1 TCP Listener Channel

A TCP Listener Channel shall listen on a configured TCP address and port.

The operating system TCP stack handles the TCP handshake. Listener does not send application-level acknowledgement to allow a client to transmit.

A TCP Listener Channel is responsible for:

- Accepting incoming client connections
- Monitoring listener status
- Creating TCP Connection Channels

The TCP Listener Channel does not directly receive application data.

When `max_connections` (§76) is configured and reached, the listener shall reject
new incoming connections, preserve existing connections, and raise a
`ConnectionRejected` warning (§93). Reaching the limit shall not fault the
listener.

### 16.2 TCP Connection Channel

Each accepted TCP client connection shall be represented by a separate TCP Connection Channel.

Each TCP Connection Channel shall have:

- Independent Byte Stream
- Independent Display Configuration
- Independent Recording Configuration
- Independent runtime state

Data from different TCP clients shall not be merged into a common Byte Stream.

_Scope (v2.1, ADR-024; parked by user decision 2026-07-11):_ these independence
requirements are architectural — each accepted connection runs its own full
pipeline today. Their **user-facing surfacing is deferred** (Appendix A): v2.1
exposes connection channels through connect/disconnect lifecycle events only,
with no per-connection snapshot, stream view, recording, or match rules, and
`recv_buffer_bytes` (§76) is not yet applied to accepted sockets.

### 16.3 TCP Connection Persistence

TCP Connection Channels are runtime objects.

TCP Connection Channels shall not be stored in:

- Profiles
- Configuration files
- Project files

Only TCP Listener Channel configuration is persisted.

### 16.4 TCP Client Connection Model

Use Model A:

```text
TCP Listener Channel
  ├─ TCP Connection Channel: client A
  ├─ TCP Connection Channel: client B
  └─ TCP Connection Channel: client C
```

Each connection is displayed and processed separately.

---

# Part VI — Stream Input

## 17. Input Processing Model

A Channel processes received data as a single **verbatim byte stream**. There is no per-Channel mode switch — the stream is the only model. (The former Message Mode and its per-Channel Stream/Message toggle are removed in v2.0.)

## 18. Stream Processing

In stream processing:

- Data is processed as a continuous sequence of bytes, in arrival order.
- No Message boundaries are recognized; the bytes are never reframed or buffered into records.
- Datagram boundaries (UDP, §15) are preserved as a reception/recording detail only; they do not segment the stream for display.
- The stream is displayed (§40–46), searchable (§50.2), and recordable (§51–59).
- There is no Message Numbering and there are no per-Message timestamps; liveness is measured in bytes (§25, §166).

## 19. Message Mode

_Removed in v2.0. `Listener` has no Message Mode; all input is a verbatim stream (§17–18)._

## 20. Extraction Methods

_Removed in v2.0. There is no Message Extraction. The former `ExtractionConfig` (`Stream` / `Delimiter` / `FixedLength` / `Protocol`) and the `MessageExtractor` trait (§139) are gone; received bytes are a verbatim stream (§17–18)._

## 21. Delimiter-Based Extraction

_Removed in v2.0 (see §20)._

## 22. Fixed-Length Extraction

_Removed in v2.0 (see §20)._

## 23. Protocol-Based Extraction

_Removed in v2.0 (see §20)._

---

# Part VII — Stream Reception Facts

## 24. Message Number

_Removed in v2.0. There are no Messages, so there is no Message Numbering. Liveness is byte-based (§25, §166)._

## 25. Total Byte Count

Total Byte Count is the number of bytes a Channel has received since it was Started. It is protocol-independent, monotonic while the Channel is Running, and resets on the next Start. It anchors the byte-based liveness readouts (§166).

## 26. Chunk Arrival Time

Each received chunk/datagram carries an arrival timestamp (`ChunkTime`: wall clock + monotonic, §133). This is a reception fact, not a per-Message timestamp (there are no Messages) — it drives the activity monitor (§166), inline Mark annotations (§50.2), and, when enabled, the recording timestamp sidecar (§57).

> **Precision is not accuracy.** A finer timestamp resolution selects how finely a time is displayed and stored; it does not guarantee timing accuracy. Userland serial/UDP arrival times carry OS scheduling and driver-buffering jitter that can exceed the selected resolution. See §133.

## 27. Reception Duration

_Removed in v2.0 (a per-Message concept)._

## 28. Integrity Metadata

_Removed in v2.0. Integrity checking belonged to decoders, which are removed (§29); the `IntegrityScope` / `IntegrityStatus` / `IntegrityMetadata` types are gone._

---

# Part VIII — Decoder Architecture (removed in v2.0)

## 29. Decoder Responsibilities

_Removed in v2.0. `Listener` decodes nothing — it is a verbatim stream tool. The `Decoder` trait (§140) and all decoder selection/output/isolation rules are gone._

## 30. Decoder Selection

_Removed in v2.0 (see §29)._

## 31. Decoder Output

_Removed in v2.0 (see §29)._

## 32. Decoder Failure Isolation

_Removed in v2.0 (see §29)._

---

# Part IX — NMEA0183 Decoder (removed in v2.0)

## 33. NMEA0183 Scope

_Removed in v2.0. `Listener` no longer decodes NMEA0183. NMEA data is carried,
displayed, searched, and recorded as ordinary stream bytes. v2.2 reacquires the
`nmea0183` crate only to construct presentation-only ZDA Mark text (§50.2, ADR-025);
no received bytes are parsed through it._

## 34. NMEA Message Boundary Requirement

_Removed in v2.0 (see §33)._

## 35. NMEA Metadata

_Removed in v2.0 (see §33)._

## 36. NMEA Checksum Validation

_Removed in v2.0 (see §33)._

## 37. NMEA Validation Modes

_Removed in v2.0 (see §33)._

## 38. NMEA Proprietary Sentences

_Removed in v2.0 (see §33)._

## 39. NMEA Multi-Sentence Groups

_Removed in v2.0 (see §33)._

---

# Part X — Display and Rendering

## 40. Display Architecture

Display is presentation-only.

Display functions shall not modify:

- Incoming bytes
- Recorded data

## 41. Display Source

A Display View operates on the received **byte stream** — the verbatim sequence of bytes as received (§17–18). There is a single source; the former per-view Stream/Messages source selector is removed in v2.0. The Channel's one view renders that stream in its selected mode, switchable at any time (§42, §48).

## 42. Display Modes

Supported Display Modes:

```rust
enum DisplayMode {
    Raw,
    Rendered,
    Hex,
}
```

## 43. Raw Display

Raw Display shall present received data as represented by the selected Display Encoding.

Raw Display shall:

- Not perform newline interpretation.
- Not perform carriage-return interpretation.
- Not perform tab expansion.
- Not perform terminal-style cursor movement.
- Not perform whitespace expansion.
- Display all bytes using configured character rendering.
- Allow the space character to be replaced by glyph or escape sequence.
- Wrap at the display boundary when wrapping is enabled.
- Not insert structure of its own (no synthesized boundaries or separators).

Raw Display is intended for forensic inspection.

## 44. Rendered Display

Rendered Display presents received data as text using terminal-style rendering rules.

Rendered Display may apply:

- Newline interpretation
- Carriage-return interpretation
- Tab expansion
- Control-character handling

## 45. Hex Display

Hex Display shall display each byte as two uppercase hexadecimal digits.

Grouping and spacing are configurable per view (`HexGrouping`, §78/§80.1): the
number of bytes per group and groups per line (0 = fit to the display width).

## 46. Character Rendering

Character Rendering options shall include:

- Native
- Token
- Glyph
- Hex Escape

Examples:

```text
[CR], [LF], [TAB], [NUL], [SP]
␍, ␊, ␉, ␀, ␠
<0D>, <0A>, <09>, <00>, <20>
```

## 47. Display Configuration

Display View configuration may include:

- Display Mode
- Display Encoding
- Font
- Foreground Color
- Background Color
- Wrapping Mode
- Character Rendering

## 48. Display View

_Amended in v2.1 (ADR-023): one logical Display View per Channel._

Each Channel has exactly **one** Display View. Raw, Rendered, and Hex are that
view's switchable **modes** (§42), not separate simultaneous views; the per-view
settings of §46–§47 and the pause of §50 apply to this single view, so "the view
is paused" and "the Channel's display is paused" mean the same thing.

The profile schema's `views` list (§78) is retained for forward compatibility;
v2.1 reads exactly one entry and ignores the rest. Multiple simultaneous views
(e.g. Raw + Hex side by side, each independently paused) are deferred —
Appendix A.

## 49. Metadata Display

_Removed in v2.0. There are no Messages and no per-Message metadata to display; a view shows the stream bytes only._

## 50. Display Pause

When Display is Paused:

- Channel operation continues (reception is never interrupted).
- Recording continues if enabled.
- The view stops accumulating new stream bytes; on Resume it continues from live data, with no backfill of the gap.

## 50.1 Sink Subsampling

_Removed in v2.0. Subsampling was a Message-oriented filter (`.dat` → `.ssdat`); with no Messages it does not apply. Raw recording is byte-exact and full-fidelity-or-off (§53); Display recording captures the rendered stream (§54)._

## 50.2 Find and Triggers

A **Match Rule** scans the received **byte stream** with a predicate; on a match it fires one or more **Actions**. Rules are per-Channel and **presentation/control only** — they never modify the stream, recordings, or reception (§40, §103, §116).

**Match conditions** (one condition per rule; compound AND/OR/sequence logic is deferred):

```rust
enum MatchCondition {
    BytePattern { pattern: Vec<u8> },   // a byte/hex/text sequence, scanned across the stream
    Idle { timeout: Duration },         // no data received for `timeout` (timer-based)
}
```

`BytePattern` scans the live stream and matches **across receive-chunk boundaries** (a pattern split between two reads still matches): the runtime retains the previous chunk's tail (the longest enabled pattern minus one byte) and scans it joined to each new chunk, reporting matches once. A match that only completes because of that carry — its first byte lay in the previous chunk — is a **boundary split**: it is recovered (not lost), its firing anchors on the match's true start offset, and it is **measured** so an operator can see read boundaries splitting their patterns. Each boundary split records an event Diagnostic naming *where* (the stream offset) and *why* (the chunk-boundary split), and increments a monotonic `match_boundary_saves` counter exposed in the Channel's stats and snapshot (*how often*). `Idle` uses the per-Channel activity monitor (§166): it fires once when the stream has been quiet for `timeout` and re-arms when data resumes. (The former `DecodedField` and `MessageSize` conditions are removed with the decoder/message infrastructure.)

**Actions:**

```rust
enum MatchAction {
    Record { target: RecordTarget, control: RecordControl }, // begin/stop, from the match forward
    Mark { timestamp: Option<MarkTimestamp> },               // marker + optional inline arrival annotation
    Notify { severity: DiagnosticSeverity },                 // raise an event/warning (§92–94)
    PauseDisplay { view: Option<DisplayViewId> },            // freeze a view, or all
}

enum RecordTarget { Raw, Display, Both }
enum RecordControl { Begin, Stop }

// The inline arrival-time annotation a Mark splices next to the matched pattern.
struct MarkTimestamp {
    position: MarkPosition,
    style: MarkTimestampStyle,
    format: TimestampConfig,
    separator: String,
}
enum MarkPosition { Before, After }
enum MarkTimestampStyle { Plain, NmeaZda { talker: String } }
// Plain mirrors talker's toggles in local time; ZDA reads include_millis only.
struct TimestampConfig { include_date: bool, include_millis: bool, include_timezone: bool }
```

- **Record** begins or stops recording **from the match forward**; there is **no pre-match backfill** (§158). Pre-trigger capture is deferred.
- **Mark** correlates the match into the display and the Display Recording (`.disp`), plus a tagged event (§137). It is **never** written into the raw `.raw` byte stream, which stays byte-exact (§49, §53). A bare `Mark` (`timestamp = None`) writes a `‹MARK …›` marker line into the view's `.disp`. When `timestamp` is `Some`, `Plain` splices the matched chunk's local arrival time, formatted per `TimestampConfig` (time-of-day always shown; date, milliseconds, and local UTC offset independently toggleable). `NmeaZda` instead splices `$<talker>ZDA,<hhmmss[.sss]>,<dd>,<mm>,<yyyy>,<zone-hours>,<zone-minutes>*XX`: time/date are UTC and zone fields carry the local offset. An offset outside minute precision or the supported ±13:59 range leaves both zone fields empty. Standard two-character talker IDs and custom 1–32-character printable ASCII IDs are accepted; whitespace/control characters and NMEA framing characters `$ ! , *` are invalid. IDs longer than two produce explicitly custom ZDA-shaped output.
- `Before` attaches immediately before the match's first byte; `After` attaches immediately after its final byte. `separator` is appended after either style and may contain CR/LF. Rendering honors those controls and resets Raw wrap state, Rendered tab columns, and Hex continuation before subsequent data. The style-generated ZDA has no trailing CR/LF of its own. All styles are text insertions in every view mode, not on-screen byte-range styling (contrast removed `Highlight`, ADR-015).
- **Notify** raises a diagnostic event/warning (§92–94). **PauseDisplay** freezes a view (or all views, §50); reception and recording continue.

**Configuration** (persists in profiles):

```rust
struct MatchRule {
    name: String,
    condition: MatchCondition,
    actions: Vec<MatchAction>,
    enabled: bool,
}
```

Rules are part of `ChannelConfig` (§72) as `match_rules: Vec<MatchRule>`.

**Evaluation and constraints.** `BytePattern` rules evaluate as bytes arrive (the scanner keeps a carry of up to `pattern.len() − 1` bytes across chunks); `Idle` runs on a timer. Evaluation is bounded and shall not stall reception (§100). One condition per rule; no cross-channel rules; no pre-trigger capture — all deferred.

---

# Part XI — Recording

## 51. Recording Overview

Recording is optional persistent output associated with a Channel. Recording is
not a Channel state; a Channel may be Running with recording Disabled, Enabled,
or Faulted (§12).

There are two independent recording systems, with different inputs, pipeline
positions, and guarantees:

- **Raw Recording** — byte-oriented; taps the received-chunk stream directly.
- **Display Recording** — rendered-output-oriented; consumes a display view's
  output *after* rendering.

Each is configured, queued, and faulted independently per Channel. Neither is an
inline pipeline stage. Both are concurrent consumers and shall never stall
reception (§100).

## 52. Recording Modes

Raw Recording (§53) and Display Recording (§54) are configured **independently** —
each is enabled or disabled on its own, with its own destination and options (§79,
ADR-013). There is no single `RecordingMode` enum: a Channel runs neither, either, or
both at once, and each has its own state, queue, file, and fault status. Raw is the
primary recording; Display is optional.

## 53. Raw Recording

Raw Recording writes received bytes exactly as received.

**Position:** it consumes the transport's `ReceivedData` chunk stream directly. It does not depend on message boundaries (there are none, §17–18).

Up to its truncation point (§56.1), Raw Recording shall preserve:

- Byte values
- Byte ordering
- Datagram / chunk ordering

The byte-stream artifact shall contain received payload bytes only. Optional
timestamps (§57) shall **not** be interleaved into the byte stream — doing so
would destroy byte-exactness. When enabled, timestamps are written to a separate
sidecar index keyed by byte offset.

Raw Recording is authoritative for the data it contains. It is **not** guaranteed
complete under sustained overload — see §56.1.

A raw recording is written with the **`.raw`** extension (§59) and is always full-fidelity — byte-exact and contiguous — or off. There is no subsampling (§50.1 removed): `.raw` replaces the former `.dat`, and the message-framed `.ssdat` variant is removed.

## 54. Display Recording

Display Recording writes the rendered output of a specific display view
(`RenderedOutput`, §141), after rendering. It is a fan-out consumer (§102),
post-render.

It may include character-rendering transformations, visible control-character
representations, and display formatting. It is explicitly not byte-exact and is
not a substitute for Raw Recording. Because the artifact is already formatted, any
inline arrival annotations a `Mark` produces (§50.2) are already spliced into rendered
text — the recorder writes what the display shows, verbatim, with no timestamp logic
of its own. (There is no separate per-chunk Display-Recording timestamp; the only
timestamping is the per-match Mark, §50.2.)

A Display Recording is bound to one display view's configuration. Recording a
different view requires a separate Display Recording.

## 55. Recording Start Behavior

Recording captures only data received while recording is Enabled. On enable:

- Previously received data shall not be backfilled from retention buffers or
  display history.
- The destination file is validated against the configured `OverwritePolicy`
  (§79). If the file exists and the policy forbids overwrite, enabling **fails**:
  recording remains Disabled and an error is surfaced. No existing file is
  clobbered (§121).
- The destination must not already be in use by another recording (§121, ADR-014).
  The recorder takes an OS advisory exclusive lock on the destination; if it cannot
  (another Channel here, or a second `listener` process, holds it), enabling **fails**
  as a recording fault and recording remains Disabled. This is a *recording* fault, not
  a Channel fault: reception continues and the Channel stays Running.

## 56. Recording Stop and Fault Behavior

Recording stops in three ways: user disable, Channel stop, or fault. In all
three, data received after the stopping instant is not written, and the file is
finalized (flushed and closed) with whatever was durably written.

```rust
enum RecordingStopReason {
    Disabled,        // user disabled recording
    ChannelStopped,  // channel left the Running state
    Faulted(RecordError),
}
```

### 56.1 Overload and Fault Semantics (acquisition-priority tiebreak)

Each recorder drains its own bounded queue, fed by the chunk tap (raw) or the
fan-out (display). When that queue cannot accept new items because the recorder
is not draining fast enough (slow disk, etc.):

- The producer shall **not** block reception to wait for the recorder. A
  recorder queue uses non-blocking enqueue. Only the Transport→Pipeline queue
  may exert backpressure on the reader (ADR-001, §97.1; §99).
- The recorder transitions to `Faulted`. Reception, display,
  retention, and the *other* recorder continue unaffected (§96).
- A faulted recording shall **not** silently gap and then resume. It is
  contiguous from start to a single truncation point, then ends. `Faulted` is
  terminal until the user re-enables recording, which begins a *new* artifact.
- On fault, the recorder records the truncation point — total bytes written
  (raw) or last rendered line written (display), plus wall-clock and monotonic
  time — finalizes the file, emits `RecordingFaulted(ChannelId)` (§137), and
  raises a diagnostic (§101) naming the channel, time, and estimated loss where
  practical.

This is the explicit resolution of §5.6 (raw recording authoritative) versus
§100 (reception highest priority): under sustained overload Listener preserves
reception and faults the recording rather than stalling the reader or writing a
gapped file. A raw recording is therefore guaranteed **contiguous and byte-exact
for the data it contains, with a known end** — not guaranteed complete.

### 56.2 Disk-Space Guard

To protect long-duration recording (extended logging, §123), a recording may
configure a **disk-space guard** (`DiskGuard`, §79/§80.1) on its destination
filesystem:

```rust
struct DiskGuard {
    min_free: Option<DiskThreshold>,  // minimum free space (bytes or percent)
    on_low: LowDiskAction,            // Warn | StopRecording
}
```

Free space is **polled periodically**, not checked on every write. When it falls
below the threshold Listener raises a warning (§93); if `on_low` is
`StopRecording`, the recorder finalizes the current file cleanly (§56) and stops,
emitting `RecordingStoppedLowDisk` (§137) while **reception continues** (§96).
Status/snapshot expose the current recording file, bytes written, free space, and —
when rotation is configured (§59) — the time remaining until the next rotation.

## 57. Timestamp Recording

Timestamp recording is optional and is the only metadata a recorder writes.
Rationale: the recorded bytes are self-sufficient; original timing is the one
fact that cannot be regenerated from them.

- **Raw Recording:** timestamps are `ChunkTime` values (wall clock + monotonic)
  written to a **sidecar index** (`.raw.idx`) keyed by byte offset; the `.raw` byte
  stream stays pure and byte-exact. The sidecar is written when
  `RawRecordingConfig.timestamp_enabled` is set — currently a config-only flag with no
  UI toggle yet (a TODO tracks exposing it).
- **Display Recording:** the recorder writes the rendered text verbatim and adds **no**
  timestamps of its own. The only display timestamp is the per-match `Mark` timestamp
  (§50.2), which is spliced inline into the rendered text *before* the recorder sees it
  — so it appears identically in the live view and the `.disp` file.

No other metadata shall be recorded by default.

## 58. Recording During Display Pause

Display Pause (§11, per display view) shall not affect either recording system.
Raw Recording is entirely upstream of display. Display Recording consumes
rendered output independently of whether a view is presented on screen; pausing a
view's presentation does not pause its recording.

## 59. Recording File Management

Recording supports user-configurable **time-based file rotation**. (Size-based
rotation and automatic pruning of old files remain deferred — Appendix A.)

**Rotation period:**

```rust
enum FileRotationPolicy {
    None,    // single file (the destination is a file)
    Hourly,
    Daily,
}
```

When `None`, the recording writes to a single destination file. When `Hourly` or
`Daily`, the `destination` (§79) is treated as a **directory** and Listener
generates a new file at the start of each period.

**Generated filenames** are `<channel-name>_<start-time><ext>`, where:

- `<start-time>` is the period's start in **local time** (the operator's wall
  clock), at the **least resolution the period needs**: `Daily` → `YYYY-MM-DD`;
  `Hourly` → `YYYY-MM-DD_HH`. (Local rather than UTC so filenames match the clock
  the operator reads; the DST edge — a repeated/skipped local hour at the change — is
  accepted and harmless to the byte stream.)
- `<ext>` identifies the file's contents:
  - **`.raw`** — raw data, byte-exact and contiguous (§53).
  - **`.disp`** — Display Recording (rendered text, §54).
  - **`.log`** — diagnostic/event log only (§114); not a Channel recording.

Example (Hourly, channel "GPS"): `GPS_2026-06-03_08.raw`.

**Rotation boundary.** At each period boundary Listener finalizes the current file
(flush + close, §56) and opens the next. A rotation is a clean **file boundary, not
a gap**: each file stays contiguous and byte-exact for the data it contains (§56),
and recording does not backfill across the boundary (§158).

**Channel-name constraints.** Because the channel name appears in filenames,
`ChannelName` is validated **filesystem-safe** at configuration time (§71):
non-empty, no path separators or reserved characters, a bounded length, and not a
reserved device name (e.g. Windows `CON`, `PRN`, `COMx`). Invalid names are
**rejected** by validation, not silently sanitized.

Still deferred (Appendix A): size-based rotation, filename templating beyond this
scheme, and a retention policy that prunes old recording files.

---

# Part XII — Export

## 60. Export Overview

Export is an on-demand operation derived from currently retained runtime information.

Export is distinct from Recording.

## 61. Export Sources

Exports may operate on:

- Retained Stream Data
- Retained Events
- Retained Errors and Warnings

## 62. Export Types

Supported Version 1 export types:

- Raw Export
- Display Export

## 63. Export Limits

Export operates only on currently retained runtime information.

Evicted data is not exportable unless it was separately recorded.

## 64. Deferred Export Features

Deferred:

- CSV export
- JSON export
- Structured semantic export
- Session replay

---

# Part XIII — Configuration and Profiles

## 65. Configuration Overview

Configuration consists of user-defined settings that determine Listener behavior.

Configuration shall be separated from Runtime Objects.

## 66. Configuration Categories

Configuration categories:

- Channel Configuration
- Interface Configuration
- Display Configuration
- Recording Configuration
- Retention Configuration

## 67. Profile Model

A Profile is a persisted collection of Listener configuration settings.

A Profile represents a complete Listener workspace.

Profiles shall not contain Runtime Objects.

## 68. Profile Contents

Profiles may contain:

- Serial Channel configuration
- UDP Channel configuration
- TCP Listener Channel configuration
- Display configuration
- Recording configuration
- Retention configuration
- Find/trigger rules (§50.2)

## 69. Profiles Exclude

Profiles shall not contain:

- TCP Connection Channels
- Active connections
- Display History (stream scrollback)
- Active Recordings
- Channel Start/Stop state
- Recording Enabled/Faulted runtime state

## 70. Profile Load Behavior

Loading a Profile shall:

- Restore configured Channels.
- Restore display configuration.
- Restore recording configuration.
- Restore find/trigger rules.

Loading a Profile shall not:

- Start Channels.
- Open interfaces.
- Begin recording.
- Restore runtime state.

All Channels shall restore in Stopped state.

Recording shall restore as Disabled.

## 71. Configuration Validation

Profile loading shall validate configuration without Starting Channels.

Invalid configuration in one Channel shall not prevent loading valid Channels.

Examples:

```text
GPS Receiver       Invalid: COM4 missing
AIS Receiver       Valid
UDP Feed           Valid
TCP Listener       Valid
```

Resources that cannot be validated until Start shall be validated during Start.

Validation shall also reject a `ChannelName` that is not **filesystem-safe**
(§59), because the name is used to generate recording filenames: a name is rejected
if it is empty, contains path separators or reserved characters, exceeds a bounded
length, or is a reserved device name (e.g. Windows `CON`, `PRN`, `COMx`). Names are
rejected, not silently sanitized.

Validation shall also reject a `ChannelName` that **duplicates** another Channel's
name in the same workspace (§6, ADR-014): names must be unique because they generate
recording filenames (§59). On profile load, a duplicate-named Channel is rejected
like any other invalid Channel (valid Channels still load). The GUI prevents
duplicates at add-channel and rename time.

---

# Part XIV — Configuration Schema

## 72. Recommended Rust Profile Structures

```rust
struct Profile {
    schema_version: u32,
    name: String,
    channels: Vec<ChannelConfig>,
    defaults: DefaultConfig,
}
```

```rust
struct ChannelConfig {
    id: Option<StableConfigId>,
    name: ChannelName,
    kind: ChannelKind,
    interface: InterfaceConfig,
    display: DisplayConfig,
    raw_recording: RawRecordingConfig,         // §79 (ADR-013) — verbatim .raw
    display_recording: DisplayRecordingConfig, // §79 (ADR-013) — rendered .disp
    retention: RetentionConfig,
    match_rules: Vec<MatchRule>,   // §50.2 find/triggers; default empty
    reconnect: ReconnectPolicy,    // §9.1; default disabled
}
```

The `extraction` and `decoder` fields are removed in v2.0 (there is no Message
Extraction or decoding); `schema_version` bumps for this breaking change (§72.1).

### 72.1 Schema Version Compatibility

`schema_version` is a single monotonically increasing `u32`. Each binary knows
one current schema version. On load, Listener compares the profile's
`schema_version` against the version the running binary supports:

- **Equal** — load normally.
- **Profile older than the binary** — additive changes within the same supported
  schema load through `#[serde(default)]`; a breaking older schema is refused
  unless an explicit migration for that schema has been implemented.
- **Profile newer than the binary** — refuse to load and report the version
  mismatch.

Additive fields use `#[serde(default)]` so profiles missing newly added optional
fields load cleanly without a version bump. The version increments only on a
breaking change that defaults alone cannot absorb; a version bump must also
define whether the older breaking schema is migrated or refused.

## 73. Interface Configuration

```rust
enum InterfaceConfig {
    Serial(SerialConfig),
    Udp(UdpConfig),
    TcpListener(TcpListenerConfig),
}
```

There is no persisted `TcpConnectionConfig`.

## 74. Serial Configuration

```rust
struct SerialConfig {
    port: String,
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
    rts: Option<bool>,
    dtr: Option<bool>,
}
```

## 75. UDP Configuration

```rust
struct UdpConfig {
    bind_address: String,
    port: u16,
    mode: UdpMode,
    multicast_group: Option<String>,
    multicast_interface: Option<String>, // NIC to join on (multi-homed hosts)
    recv_buffer_bytes: Option<usize>,    // SO_RCVBUF
}

enum UdpMode {
    Unicast,
    Broadcast,
    Multicast,
}
```

## 76. TCP Listener Configuration

```rust
struct TcpListenerConfig {
    bind_address: String,
    port: u16,
    max_connections: Option<u32>,
    recv_buffer_bytes: Option<usize>,    // SO_RCVBUF for accepted connections
}
```

### 76.1 Network Live Adjustment (troubleshooting)

Listener shall support live adjustment of network parameters the OS allows without
a re-bind:

- **Receive buffer (SO_RCVBUF)** — set at open and adjustable live; the primary
  lever against kernel-dropped UDP datagrams (§101 — loss the OS does not surface).
  Live changes are best-effort: some platforms only honor a buffer size set
  before/at bind, so a full change may require a restart (§13).
- **Multicast membership** — join/leave groups live, and select the join interface
  on multi-homed hosts (`JoinMulticast` / `LeaveMulticast` / `SetReceiveBuffer`,
  §136).

Parameters needing a re-bind (bind address, port) change via the §13 apply-pending
coordinated restart, not live. Live **socket state** — bound address, connected
peer(s), multicast membership, throughput/last-received — is exposed in
status/snapshot for troubleshooting.

## 77. Decoder Configuration

_Removed in v2.0. There is no decoder (§29); `DecoderConfig` is gone._

## 78. Display Configuration

```rust
struct DisplayConfig {
    views: Vec<DisplayViewConfig>,
}

struct DisplayViewConfig {
    mode: DisplayMode,                 // §42: Raw / Rendered / Hex
    encoding: DisplayEncoding,
    character_rendering: CharacterRendering,
    font: Option<String>,
    foreground_color: Option<String>,
    background_color: Option<String>,
    wrapping: WrappingMode,
    hex_grouping: HexGrouping,         // §45
}
```

The `source`, `timestamp`, `annotations`, and `subsample` fields are removed in
v2.0: there is one source (the stream, §41), no per-Message annotations or
timestamps (§49), and no subsampling (§50.1).

## 79. Recording Configuration

Raw (§53) and Display (§54) recording are **independently configured** — they tap the
pipeline at different points (Raw the verbatim byte stream, Display the rendered view)
and were always separate (listener ADR-013). Each has its own destination and options,
so a Channel may run both at once to two different files.

```rust
struct RawRecordingConfig {            // §53 — the verbatim byte stream (.raw)
    enabled: bool,                     // begin at channel Start; the live Record
                                       //   toggle (§50.2, ADR-012) begins/stops at
                                       //   runtime whenever a destination is set
    destination: Option<PathBuf>,      // a file when rotation = None; a directory otherwise (§59)
    timestamp_enabled: bool,           // §57: sidecar index
    overwrite_policy: OverwritePolicy,
    file_rotation: FileRotationPolicy, // §59; default None
    disk_guard: Option<DiskGuard>,     // §56.2 — guards long Raw captures
}

struct DisplayRecordingConfig {        // §54 — the rendered view output (.disp)
    enabled: bool,
    destination: Option<PathBuf>,
    overwrite_policy: OverwritePolicy,
    file_rotation: FileRotationPolicy, // §59; default None
}                                      // no timestamp field — the only display
                                       //   timestamp is the per-match Mark (§50.2)
```

The single `RecordingConfig`/`RecordingMode` (Disabled/Raw/Display/Both) of v2.0 is
removed: the four modes are now two `enabled` bools, and "Both" is simply both enabled
to their own destinations. The `subsample` field is removed (§50.1); raw recording is
byte-exact-or-off. Splitting the config bumps `schema_version` to 3 (a clean break —
v1/v2 profiles are refused, ADR-013).

## 80. Retention Configuration

```rust
struct RetentionConfig {
    byte_limit: Option<usize>,     // stream scrollback in bytes (replaces message_limit)
    event_limit: Option<usize>,
    warning_limit: Option<usize>,
    error_limit: Option<usize>,
}
```

At least one applicable retention limit shall be enforced. Profile validation
(§71) shall reject any `RetentionConfig` in which all retention limits are unset
(every field is `None`), because that would leave retention unbounded.
Independently, the runtime shall apply a hard implementation backstop cap so that
bounded resource usage (§124) holds even if a configuration with all retention
limits unset reaches the runtime (e.g. via a hand-edited profile loaded under
§71's per-channel isolation).

## 80.1 Supporting Enumerations and Newtypes

Central types referenced by the configuration and data structures above:

```rust
pub enum DisplayEncoding {
    Ascii,
    Utf8,
    Utf16Le,
    Utf16Be,
    Latin1,
}

pub enum CharacterRendering {
    Native,
    Token,     // [CR] [LF] [TAB] [NUL] [SP]
    Glyph,     // ␍ ␊ ␉ ␀ ␠
    HexEscape, // <0D> <0A> <09> <00> <20>
}

pub enum WrappingMode {
    NoWrap,
    Wrap,
}

pub enum OverwritePolicy {
    Refuse,         // fail enable if the file exists (default; §121)
    Overwrite,      // truncate and replace
    AppendIfExists, // append to an existing file
}

pub enum DataBits { Five, Six, Seven, Eight }
pub enum Parity { None, Even, Odd, Mark, Space }
pub enum StopBits { One, OnePointFive, Two }
pub enum FlowControl { None, RtsCts }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DisplayViewId(uuid::Uuid);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StableConfigId(uuid::Uuid);

// Profile-level defaults applied to channels that do not override them.
pub struct DefaultConfig {
    pub display: Option<DisplayConfig>,
    pub retention: Option<RetentionConfig>,
}

// (FileRotationPolicy §59, MatchRule/MatchCondition/MatchAction/RecordTarget/
//  RecordControl §50.2, DiskGuard §56.2, and SerialControlLines §14.3 are defined
//  in their body sections.)

// §45; groups_per_line 0 = fit to the display width.
pub struct HexGrouping { pub bytes_per_group: u8, pub groups_per_line: u8 }

// §50.2 — compact local-time options; NMEA ZDA reuses include_millis only.
pub struct TimestampConfig { pub include_date: bool, pub include_millis: bool, pub include_timezone: bool }

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MatchRuleId(uuid::Uuid); // §50.2

pub struct ReconnectPolicy {        // §9.1; enabled defaults false
    pub enabled: bool,
    pub initial_backoff: std::time::Duration,
    pub max_backoff: std::time::Duration,
    pub multiplier: f64,
    pub max_attempts: Option<u32>,  // None = unlimited
}

pub enum DiskThreshold { Bytes(u64), Percent(u8) } // §56.2
pub enum LowDiskAction { Warn, StopRecording }     // §56.2
```

---

# Part XV — Default Templates

## 81. Template Principles

Templates provide safe starting configurations.

Templates create valid, stopped Channel configurations.

Templates shall not automatically open interfaces.

## 82. Serial Template

Defaults:

```text
Kind: Serial
Name: Serial Channel
Baud Rate: 9600
Data Bits: 8
Parity: None
Stop Bits: 1
Flow Control: None
Recording: Disabled
Display: Raw + Hex
```

## 83. NMEA Serial Template

_Removed in v2.0. There is no NMEA-specific template — NMEA arrives as ordinary stream bytes on a Serial channel (§33). Use the Serial template (§82); a common NMEA serial port is 4800 8N1._

## 84. UDP Template

Defaults:

```text
Kind: UDP
Name: UDP Channel
Bind Address: 0.0.0.0
Recording: Disabled
Display: Raw + Hex
```

## 85. TCP Listener Template

Defaults:

```text
Kind: TCP Listener
Name: TCP Listener
Bind Address: 0.0.0.0
Max Connections: unset
Recording: Disabled
```

---

# Part XVI — Retention and Memory Management

## 86. Retained Information

Listener may retain:

- Display History (stream scrollback)
- Stream Data
- Events
- Warnings
- Errors

Retention is runtime-only.

Retention shall not affect Recording.

## 87. Display History

Display History supports scrolling and review.

Display History is independent of Recording.

## 88. Retention Limits

Retention shall be bounded.

Supported limits:

- Byte Count (stream scrollback)
- Event Count
- Warning Count
- Error Count

Memory-based limits may be implementation-specific but are not primary user-facing limits.

## 89. Eviction

When retention limits are exceeded, oldest retained items shall be discarded first.

Eviction discards the oldest stream bytes; it never reorders the bytes that remain.

### 89.1 Diagnostics Across a Channel Restart

A Channel's retained **diagnostics** (Events/Warnings/Errors, §92–§94) persist across
that Channel's own stop→start **within a session**: a restarted Channel keeps the
previous run's diagnostics log rather than starting blank, so an operator can see what
happened before and after a restart in one timeline. The §88 per-severity limits still
bound the carried-forward log (oldest evicted, §89).

This applies to a start that **faults** too (e.g. a bind conflict that never runs a
pipeline): the fault is retained as an Error diagnostic and remains visible after a
later successful restart, alongside the Channel-Started/Stopped events.

Retention remains **runtime-only** (§85): it is not persisted to disk and does not
survive closing the application. The stream **scrollback** is not carried across a
restart (a fresh run starts a new byte stream, §8.5).

## 90. Clearing Runtime Data

Users may clear retained runtime data.

Clearing retained data shall not affect:

- Recording files
- Channel configuration
- Profiles

---

# Part XVII — Status, Events, Warnings, and Errors

## 91. Status

Status describes current condition.

Examples:

- Stopped
- Starting
- Running
- Stopping
- Faulted
- Display Paused
- Recording Enabled
- Recording Faulted

### 91.1 Channel Activity and Liveness

Each Channel maintains a lightweight **activity monitor** of pure facts, updated
per received chunk and exposed in status/snapshot:

```rust
struct ChannelActivity {
    last_data_at: Option<Instant>,  // arrival of the last chunk/datagram; None until first data
    bytes_per_sec: f64,             // rolling
    total_bytes: u64,               // since Start (§25)
}
```

It answers "is data arriving?" at a glance (throughput, time-since-last-data, a
derived idle indicator). **Idle-ness is computed by each consumer against its own
threshold** — a display uses a UI threshold; a Match Rule's `Idle` condition (§50.2)
uses its own `timeout` — so there is one fact source and no competing definitions
of "idle". The monitor is bounded and shall not affect reception (§100, §124); it
is runtime-only and never persisted.

## 92. Events

Events describe something that occurred.

Examples:

- Channel Started
- Channel Stopped
- TCP Client Connected
- TCP Client Disconnected
- Recording Enabled
- Recording Disabled
- Profile Loaded
- Profile Saved

## 93. Warnings

Warnings indicate conditions that may affect operation but do not prevent operation.

Examples:

- Queue overflow
- Display backpressure
- Connection rejected (max connections reached)

## 94. Errors

Errors indicate failure to complete an operation or continue normal operation.

Examples:

- COM port not found
- COM port access denied
- TCP bind failure
- UDP bind failure
- Recording file creation failure

## 95. Error Categories

```rust
enum ErrorCategory {
    Configuration,
    Resource,
    Communication,
    Recording,
    Internal,
}
```

## 96. Failure Isolation

Failures shall be isolated to the smallest affected subsystem whenever practical.

Examples:

- Recording Failure faults Recording but does not stop Reception.
- TCP client failure does not stop the TCP Listener.
- One Channel failure does not stop other Channels.

---

# Part XVIII — Concurrency and Backpressure

## 97. Concurrency Overview

Listener shall support concurrent operation of multiple Channels.

Activity in one Channel shall not block operation of other Channels.

### 97.1 ADR-001 — Concurrency Model

Decision:

Listener uses a hybrid runtime model.

- Tokio coordinates runtime orchestration, command handling, cancellation,
  bounded queues, fan-out, async-native network I/O, and shutdown.
- Continuous blocking serial receive loops run on dedicated OS threads that own
  the underlying interface handle.
- `tokio::task::spawn_blocking` is reserved for bounded blocking operations that
  complete in finite time (interface open/close, file create, file flush, port
  enumeration). It shall not host continuous receive loops.
- Blocking transport threads send into bounded `tokio::sync::mpsc` channels using
  `Sender::blocking_send`.
- Transport abstractions are push-based. Data-bearing transports emit
  `ReceivedData`; TCP Listener transports emit `NewConnection`.
- The runtime mints `ChannelId` values for accepted TCP connections.

Consequences:

- `blocking_send` backpressure on the Transport→Pipeline queue can stall the
  serial reader, which can cause UART/driver overrun. This is the mechanism
  behind "transport-specific loss" in §99 and shall be reported per §101.
- Because every fan-out consumer downstream of the pipeline is non-blocking (drop,
  evict, or fault), the pipeline never stalls on a slow consumer; the only
  condition that can stall the reader is the CPU failing to keep up with the line
  rate.
- Continuous blocking receive loops require a bounded read timeout (or handle
  close) so they can observe cancellation; shutdown does not rely on interrupting
  an in-progress blocking read (§111).
- Timing is chunk-granular, not true per-byte hardware timing (`ChunkTime`, §138);
  recording timestamps derive from it (§26, §57).

### 97.2 Blocking-to-Async Handoff

Blocking producer threads shall hand received data to the async runtime using
bounded `tokio::sync::mpsc` channels; the producer uses `Sender::blocking_send`.
This channel is the Transport→Pipeline queue and participates in the §99
backpressure policy. Implementations shall not introduce an unbounded
intermediate queue between blocking transport threads and the async runtime.

## 98. Channel Isolation

Each Channel shall maintain independent:

- Reception
- Recording
- Retention (stream scrollback)
- Find / trigger state

## 99. Queue and Backpressure Matrix

| Queue | Bounded | Overflow Behavior |
|---|---:|---|
| Transport → Pipeline | Yes | `blocking_send`; may stall the reader (the only edge permitted to); transport-specific loss may occur and is reported (§101) |
| Fan-out → Raw Recording | Yes | `try_send`; Raw Recording faults on full; never stalls reception |
| Fan-out → Display / scrollback | Yes | Drop oldest display items |
| Fan-out → Display Recording | Yes | `try_send`; Display Recording faults on full; never stalls reception |
| Fan-out → Find / Triggers | Yes | Bounded scan; never stalls reception |
| Diagnostics | Yes | Retained **per severity** (§88): each of Events/Warnings/Errors is count-bounded, oldest-within-severity evicted (§89) |

Only the Transport→Pipeline edge may exert backpressure on the reader. Every
other edge shall drop, evict, or fault — never stall acquisition.

### 99.1 Queue Topology

```text
transport reader
  │  [bounded transport→pipeline queue; blocking_send — the only edge that may stall the reader]
  ▼
pipeline (per channel)
  │
  ▼
fan-out (non-blocking)
  ├─ raw recorder queue        [bounded; try_send — fault on full; never stalls]
  ├─ display / scrollback      [bounded; drop oldest]
  ├─ display recorder queue    [bounded; try_send — fault on full]
  ├─ find / triggers           [bounded scan]
  └─ diagnostics log           [per-severity count-bounded; oldest-within-severity evicted]
```

The transport→pipeline queue is the only edge permitted to block or stall the
reader; every fan-out consumer uses non-blocking enqueue and drops or faults rather
than stalling acquisition.

Chunk payloads shall be shared between branches using `Arc`-backed storage rather
than copied per consumer.

## 100. Reception Priority

Reception shall have highest priority.

Display lag shall not block Reception.

## 101. Data Loss Reporting

When Listener discards data due to overflow, it shall report:

- Channel Name
- Time
- Overflow Type
- Estimated Data Loss where practical

Listener shall report detectable data loss. Some loss — e.g. kernel-dropped UDP
datagrams — is not reliably observable from userland; Listener reports the loss it
can detect rather than guaranteeing detection of all loss.

---

# Part XIX — Processing Pipeline and Ownership

## 102. Processing Pipeline

All Channels share one stream pipeline. UDP datagrams and serial/TCP byte chunks differ only in how the transport reads them; both become `ReceivedData` (§138):

```text
Receive Bytes (chunk / datagram)
  ↓
Activity / liveness (byte count, last-data time)
  ↓
Fan-out (non-blocking, §99–100)
  ├─ Raw Recording (optional, byte-exact tap)
  ├─ Stream scrollback (display history)
  ├─ Display + Display Recording (rendered)
  ├─ Find / Triggers (BytePattern scan, Idle)
  └─ Diagnostics
```

There is no extraction, metadata, or decoding stage. The only edge that may backpressure the reader is the bounded transport→pipeline queue (§99); every fan-out consumer is non-blocking and may drop or fault rather than stall reception (§100).

## 103. Ownership Principle

Each stage owns only the state it creates. A received chunk is immutable once read; consumers share it without copying:

```rust
Arc<ReceivedData>
```

## 104. Transport Ownership

Transport owns:

- Serial handles
- UDP sockets
- TCP listener sockets
- TCP connection sockets
- Transport receive buffers

Transport outputs `ReceivedData` (defined in §138). Each chunk carries its
payload and a `ChunkTime` capture (monotonic + wall clock) taken when the chunk
was read.

## 105. Extractor Ownership

_Removed in v2.0. There is no extractor (§20)._

## 106. Liveness Ownership

The activity/liveness stage owns the per-Channel byte counter and last-data time (§25, §166). It derives rolling throughput and the idle signal used by `Idle` Find rules (§50.2). It creates no Message objects — there are none.

## 107. Decoder Ownership

_Removed in v2.0. There is no decoder (§29)._

## 108. Fan-Out Ownership

Fan-out distributes each immutable received chunk to independent consumers (recording, display/scrollback, find/triggers, diagnostics). No consumer owns the pipeline.

---

# Part XX — Async Shutdown Model

## 109. Shutdown Principle

Shutdown shall be explicit, ordered, cooperative, and graceful whenever possible.

Priorities:

1. Stop receiving new data.
2. Preserve already received data.
3. Flush recording output.
4. Terminate tasks cleanly.
5. Report incomplete shutdown when necessary.

## 110. Channel Stop Sequence

```text
Stop requested
  ↓
Stop transport reception
  ↓
Close interface/socket
  ↓
Finish processing queued accepted data
  ↓
Flush recording output
  ↓
Terminate channel tasks
  ↓
Enter Stopped state
```

## 111. Cancellation

Cancellation shall be cooperative.

Abrupt task termination should be reserved for unrecoverable shutdown failure.

A blocking receive loop on a dedicated OS thread (ADR-001, §97.1) cannot poll a
cancellation token while parked in a blocking read. Such loops shall use a
bounded read timeout (and/or handle close) so they periodically return to observe
cancellation; shutdown shall not rely on interrupting an in-progress blocking
read.

## 112. Partial Data During Shutdown

_There are no Messages, so nothing is "partial." At shutdown, in-flight stream bytes already accepted by a recorder are flushed (§56); transports then close (§110). Bytes still in transit may be discarded._

## 113. Application Exit

Application exit shall perform Stop on all Running Channels.

Each Channel shall shut down independently.

---

# Part XXI — Logging and Diagnostics

## 114. Logging Purpose

Diagnostic logging is for:

- Troubleshooting
- Operational review
- Debugging
- Failure diagnosis

Diagnostic logging is not Channel Recording.

## 115. Logging Levels

```rust
enum LogLevel {
    Error,
    Warning,
    Info,
    Debug,
    Trace,
}
```

Trace logging may generate high data volumes.

## 116. Log Independence

Diagnostic logging shall not modify:

- Messages
- Recordings
- Display contents
- Metadata

## 117. Logging Failures

Failure of diagnostic logging shall not terminate Listener operation whenever practical.

## 118. Persistent Diagnostic Logs

Persistent diagnostic log files are optional.

Rotation policy is deferred.

---

# Part XXII — Security and Robustness

## 119. Malformed Input

Malformed input shall not crash Listener.

Examples:

- Invalid UTF-8
- Invalid UTF-16
- Invalid NMEA sentence
- Invalid checksum field
- Unexpected binary data
- Oversized messages

## 120. Unsafe Input Assumption

All received data is untrusted.

Received data shall not be executed, interpreted as commands, or used to alter Listener configuration.

## 121. File Safety

Listener shall avoid accidental data loss when writing files.

Default behavior shall avoid overwriting existing files without explicit user confirmation or configured overwrite policy.

**No two recordings to one file (ADR-014).** A recording destination shall not be
written by more than one recording at a time. Two concurrent live writers would
interleave and corrupt a capture — a failure the `OverwritePolicy` does not prevent
(it only guards a *pre-existing* file). This is enforced in two layers: (1) unique
Channel Names (§6, §71) make the rotating-filename collision impossible by
construction; (2) an OS **advisory exclusive lock** held for the recording's lifetime
is the race-free enforcement point — it catches two Channels here *and* a second
`listener` process, and a lock conflict is surfaced as a *recording* fault that leaves
the Channel Running (reception continues; recording stays off). On a local filesystem
this is robust on Windows, macOS, and Linux. Residual gaps (identical on every OS): an
unrelated external program that writes without locking is not blocked on Unix (advisory
locks), and advisory locks over a network filesystem are unreliable.

## 122. Privileged Resources

If an operation requires elevated privileges, Listener shall report the requirement clearly.

Examples:

- Binding privileged network ports
- Accessing restricted serial devices
- Writing to protected directories

---

# Part XXIII — Performance and Determinism

## 123. Long-Duration Operation

Listener shall support extended continuous operation without requiring periodic application restart.

## 124. Bounded Resource Usage

Listener shall avoid unbounded growth of:

- Memory
- Queues
- Retained history
- Runtime metadata
- Internal buffers

## 125. Deterministic Ordering

Stream bytes within a Channel shall preserve receive order.

Listener shall not reorder received data within a Channel.

If wall-clock timestamps are equal, the monotonic capture order (§133) determines ordering.

## 126. Undefined Timing Guarantees

Listener does not guarantee:

- Real-time scheduling
- Hard real-time latency
- Deterministic OS scheduling
- Deterministic network latency

Listener is a monitoring and analysis tool, not a hard real-time control system.

---

# Part XXIV — Rust Implementation Architecture

## 127. Crate Layout (resolved — single crate, modular internals)

**Decision (OQ-L1):** `Listener` ships as a **single `listener` crate** inside the
shared workspace, not the multi-crate split this section originally proposed. The
twelve `listener-*` names below are realized as **modules** under `src/`, not as
separate member crates. The module boundaries and dependency direction are exactly
those described in §128; only the packaging is collapsed. A crate split remains
available later (extract a module into a `listener-*` member crate when an external
consumer or a compile-time concern justifies it) without disturbing the module API.

This also corrects the original sketch, which nested `nmea0183/` inside `listener`.
In this workspace `nmea0183` is a **top-level sibling crate**, shared with `talker`.
ADR-010 removed Listener's decoder and its original dependency. v2.2 adds a narrowly
scoped construction-only dependency for ZDA Mark presentation text (ADR-025); it does
not restore decoding (§29).

```text
wiredata/                    # workspace root
  Cargo.toml                 # workspace manifest
  nmea0183/                  # sibling crate (shared with talker)
  talker/                    # sibling crate
  listener/
    Cargo.toml
    docs/                    # spec, ADR, TODO
    src/
      main.rs                # thin shim: dispatch to CLI or GUI
      lib.rs                 # declares + re-exports the modules below
      core/                  # → listener-core
      transport/             # → listener-transport
      display/               # → listener-display
      record/                # → listener-record
      retention/             # → listener-retention
      config/                # → listener-config
      diagnostics/           # → listener-diagnostics
      runtime/               # → listener-runtime
      cli/                   # → listener-cli (presentation only)
      gui/                   # → listener-gui (presentation only)
```

The `lib.rs` + thin-`main.rs` shape matches `talker`'s convention (talker ADR-014):
core logic lives in modules that are unit-testable through the library crate, and the
binary only dispatches.

## 128. Module Responsibilities

The boundaries below are normative regardless of whether each lives in its own crate
(future) or as a module (current, per §127). Each heading names the conceptual unit
(`listener-*`) and its current module path.

### listener-core  (`src/core/`)

Owns:

- Common types
- `ReceivedData` / `ChunkTime`
- Channel IDs
- State enums
- Shared errors

Avoids:

- IO
- UI
- Async runtime coupling where practical

### listener-transport  (`src/transport/`)

Owns:

- Serial transport
- UDP transport
- TCP listener transport
- TCP connection transport

Does not know about:

- Display
- Recording format

### listener-extract — removed in v2.0

There is no extraction module; received bytes are a verbatim stream (§20).

### listener-decode — removed in v2.0

There is no decoder module. The `nmea0183` dependency is construction-only for ZDA
Mark presentation text and never receives stream bytes (§29, ADR-025).

### listener-display  (`src/display/`)

Owns:

- Raw rendering
- Rendered rendering
- Hex rendering
- Character rendering
- Wrapping

### listener-record  (`src/record/`)

Owns:

- Raw recording
- Display recording
- File lifecycle
- Flush behavior

### listener-retention  (`src/retention/`)

Owns:

- Bounded history
- Eviction policies
- Retained display/message/event data

### listener-config  (`src/config/`)

Owns:

- Profile schema
- Configuration load/save
- Validation
- Templates

### listener-diagnostics  (`src/diagnostics/`)

Owns:

- Events
- Warnings
- Errors
- Diagnostic logging

### listener-runtime  (`src/runtime/`)

Owns orchestration:

- Channel lifecycle
- Task spawning
- Queue wiring
- Shutdown
- Fan-out
- Backpressure policy

### listener-cli and listener-gui  (`src/cli/`, `src/gui/`)

Presentation layers only.

They issue commands and observe runtime state.

They do not implement core behavior.

---

# Part XXV — Recommended Rust Data Structures

## 129. Channel Identity

```rust
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(uuid::Uuid);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelName(String);
```

## 130. Channel Kind and States

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Serial,
    Udp,
    TcpListener,
    TcpConnection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Faulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayState {
    Active,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingState {
    Disabled,
    Enabled,
    Faulted,
}
```

## 131. Message

_Removed in v2.0. There are no Messages; received data is a verbatim stream (§17–18)._

## 132. Message Metadata

_Removed in v2.0 (see §131)._

## 133. Timestamp

Internal timing uses one model, not strings: a monotonic `Instant` for ordering and the §125 tie-break, and a wall-clock `SystemTime` for display. The unit is the per-chunk `ChunkTime` (§138); the raw recording timestamp sidecar (§57), inline Mark annotations (§50.2), and the activity monitor (§166) all derive from it. Formatting happens at the Mark/rendering boundary, not in stored state.

The only display timestamp is the per-match **Mark** annotation (§50.2). `Plain` is formatted in **local** time by a toggleable `TimestampConfig` (mirroring talker's): time-of-day is always shown; date, milliseconds, and local UTC offset are independently toggleable. `NmeaZda` carries UTC time/date plus local-zone fields and uses only the millisecond toggle.

```rust
// Compact local-time options for a Mark (§50.2); ZDA reads include_millis only.
pub struct TimestampConfig { pub include_date: bool, pub include_millis: bool, pub include_timezone: bool }
```

Timestamp *resolution* is a display/storage precision choice, not an accuracy guarantee; userland serial/UDP arrival times carry OS scheduling jitter that may exceed the displayed resolution.

## 134. Protocol Metadata

_Removed in v2.0. There is no decoder to produce protocol metadata (§29)._

## 135. Integrity Metadata

_Removed in v2.0 (see §28, §134)._

## 136. Runtime Commands

Commands flow UI → runtime as direct `Listener` async method calls — there is no
`RuntimeCommand` enum (listener ADR-012). The orchestrator exposes one method per
operation: `start` / `stop` / `apply_pending` (§13), `enable_recording` /
`disable_recording`, `pause_display` / `resume_display` (§11), and the v1.2 live
controls `set_rts` / `set_dtr` (§14.3). Network live-adjustment (§76.1 — multicast
join/leave, receive-buffer) and match-rule commands (`SetMatchRuleEnabled`, `MarkNow`,
§50.2) land as further methods plus an internal command channel into the per-Channel
pipeline (deferred, ADR-008).

The GUI's `UiCommand` (`gui::bridge`) is the on-the-wire command form across the
App↔driver channel; the driver translates each into the matching `Listener` call
(ADR-008). It is richer than the orchestrator API where the GUI needs it
(`AddChannel`, `RemoveChannel`, `Rename`, `Reconfigure`, `Select`, profile save/load).

## 137. Runtime Events

```rust
#[non_exhaustive]
pub enum RuntimeEvent {
    ChannelStarted(ChannelId),
    ChannelStopped(ChannelId),
    ChannelFaulted(ChannelId),
    RecordingFaulted(ChannelId),
    RecordingStarted(ChannelId),              // a Raw recording began OK (§50.2); lets observers clear a prior fault
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
    // v1.2 additions
    ReceptionStalled(ChannelId, Duration),    // §101/ADR-007: dedicated, replaces the WarningRaised overload
    ControlLinesChanged(ChannelId),           // §14.3: input CTS/DSR/DCD/RI changed — poll snapshot for detail
    MatchTriggered(ChannelId, MatchRuleId),   // §50.2: a rule fired (Notify/Mark observable here)
    ChannelReconnecting(ChannelId, u32 /*attempt*/), // §9.1
    ChannelReconnected(ChannelId),
    ChannelReconnectGaveUp(ChannelId),
    DiskSpaceLow(ChannelId),                  // §56.2
    RecordingStoppedLowDisk(ChannelId),
}
```

---

# Part XXVI — Recommended Public Traits

## 138. Transport Contracts (push-based)

Per ADR-001 (§97.1), transports are push-based and split by output shape. These
are internal runtime contracts, not stable public traits — a public transport
trait would become a plugin seam, and plugin architecture is deferred
(Appendix A). The required shape is what matters, not the exact spelling: data
sources emit `ReceivedData`; connection acceptors emit `NewConnection`.

```rust
// Data-bearing sources: Serial, UDP, TCP Connection.
// Serial runs its body on a dedicated OS thread; UDP/TCP on a Tokio task.
pub trait DataTransportRunner {
    fn run(
        self,
        out: tokio::sync::mpsc::Sender<ReceivedData>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> TransportJoinHandle;
}

// Connection-accepting source: TCP Listener. Not a data source.
pub trait ConnectionAcceptorRunner {
    fn run(
        self,
        out: tokio::sync::mpsc::Sender<NewConnection>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> TransportJoinHandle;
}
```

There is no pull-based `receive()`. Lifecycle is cancel-then-await: the runtime
cancels the token, then awaits completion via `TransportJoinHandle`.

`TransportJoinHandle` unifies a Tokio task handle and a dedicated OS thread:

- Tokio tasks return their `tokio::task::JoinHandle`.
- Dedicated serial threads signal completion through an async-observable
  mechanism (e.g. a `oneshot`); joining a blocking thread shall not block a
  runtime worker.

```rust
pub struct ReceivedData {
    pub channel_id: ChannelId,
    pub payload: ReceivedPayload,
    pub received_at: ChunkTime,
}

pub enum ReceivedPayload {
    Bytes(Vec<u8>),    // a stream chunk (serial / TCP)
    Datagram(Vec<u8>), // one UDP datagram; boundary preserved as a recording detail (§15)
}

// Single capture per chunk. Monotonic for ordering/duration/tie-break (§125);
// wall clock for Local/UTC display (§26). Per-byte arrival time is not available
// from the OS and is not represented.
pub struct ChunkTime {
    pub monotonic: std::time::Instant,
    pub wall_clock: std::time::SystemTime,
}

pub struct NewConnection {
    pub listener_channel_id: ChannelId,
    pub remote_addr: std::net::SocketAddr,
    pub accepted_at: ChunkTime,
    // The emitted value also carries the accepted stream (the reason it exists);
    // remote_addr / accepted_at are metadata, not identity. The runtime mints the
    // new connection's ChannelId on receipt.
}
```

## 139. Message Extractor Trait

_Removed in v2.0. There is no extractor (§20)._

## 140. Decoder Trait

_Removed in v2.0. There is no decoder (§29)._

## 141. Renderer Trait

```rust
pub trait Renderer {
    // Render a span of stream bytes under the chosen Display Mode + character
    // rendering (§42–46) to display/recording text. There are no Messages.
    fn render(&self, bytes: &[u8]) -> RenderedOutput;
}
```

## 142. Recorder Traits

Two writers match the two recording systems (§51): a byte-exact raw writer and a rendered display writer.

```rust
#[async_trait::async_trait]
pub trait RawRecorder {
    /// Append one received chunk exactly as received.
    async fn write_chunk(&mut self, chunk: &ReceivedData) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    /// Flush, close, and write the truncation marker (for faults).
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}

#[async_trait::async_trait]
pub trait DisplayRecorder {
    async fn write_rendered(&mut self, output: &RenderedOutput) -> Result<(), RecordError>;
    async fn flush(&mut self) -> Result<(), RecordError>;
    async fn finalize(&mut self, reason: RecordingStopReason) -> Result<(), RecordError>;
}
```

Both implementations run as their own task draining a bounded queue (file I/O via
`spawn_blocking` or `tokio::fs`, buffered — ADR-001, §97.1). Queue-full →
transition to `Faulted` and call `finalize(Faulted(..))`; never block the
producer (§56.1).

## 143. Retention Store Trait

```rust
pub trait RetentionStore<T> {
    fn push(&mut self, item: T);
    fn clear(&mut self);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
}
```

---

# Part XXVII — VS Code / Codex Development Setup

## 144. Recommended Tooling

Use:

- Rust stable toolchain
- VS Code
- rust-analyzer
- CodeLLDB
- Markdown All in One
- Codex extension
- Git

## 145. Rust Components

Install:

```bash
rustup component add clippy
rustup component add rustfmt
```

## 146. VS Code Settings

Recommended `.vscode/settings.json`:

```json
{
    "rust-analyzer.check.command": "clippy",
    "editor.formatOnSave": true,
    "[rust]": {
        "editor.defaultFormatter": "rust-lang.rust-analyzer"
    }
}
```

## 147. Development Order

Recommended implementation order (v2.0; for the v1→v2 strip, follow ADR-010's build order):

1. `listener-core` (`ReceivedData`, `ChunkTime`, channel IDs/states)
2. transport contracts + queue/backpressure tests (the single backpressure edge, §99)
3. `listener-runtime` stream pipeline skeleton (fan-out, failure isolation)
4. byte-bounded retention / stream scrollback
5. UDP transport
6. serial transport
7. TCP listener/connection transport
8. raw recording (`.raw`), then rendering + display recording (`.disp`)
9. stream find/triggers (`BytePattern` across chunks, `Idle`)
10. config/schema + profiles (v1 refusal)
11. CLI, then GUI

Do not start with GUI.

---

# Part XXVIII — Testing and Verification

## 148. Testing Principle

Observable behavior matters more than implementation details.

Given identical input and configuration, Listener shall produce deterministic:

- Stream byte ordering
- Chunk / datagram ordering
- Find/trigger match results (byte offsets)
- Raw recording output

## 149. Unit Test Placement

Unit tests should live in `#[cfg(test)]` modules at the bottom of the file under test.

Integration tests shall live in the crate `tests/` directory.

## 150. Required Test Areas

Required tests:

- Byte-pattern find across receive-chunk boundaries (§50.2)
- Idle-rule firing and re-arming
- UDP datagram boundary preservation
- Stream-scrollback eviction (byte-bounded, ordered)
- Raw recording byte-exactness
- Display recording reflects the rendered view
- File rotation (hourly/daily, clean boundary, filesystem-safe name)
- Display Pause behavior
- TCP Connection Channel creation
- State transitions
- Queue overflow handling (single backpressure edge; consumers drop/fault)
- Failure isolation
- Graceful shutdown

## 151. NMEA Tests

_Removed in v2.0. There is no NMEA decoder to test (§29); NMEA data is exercised as ordinary stream bytes by the stream display/recording tests._

## 152. Backpressure Tests

Backpressure tests shall verify:

- Display/scrollback queue overflow does not stop reception.
- Recording failure faults recording but not reception.
- Stream-scrollback eviction is byte-bounded and preserves order.
- Diagnostics overflow drops low-priority entries first.

---

# Part XXIX — Acceptance Criteria

## 153. Channel Operation

A user shall be able to:

- Create Serial, UDP, and TCP Listener Channels from templates.
- Configure the parameters of the transport selected when each Channel was created.
- Start and Stop Channels independently.
- Run multiple Channels simultaneously.
- View Channel status.

## 154. TCP Connections

A TCP Listener shall:

- Accept incoming clients.
- Create one TCP Connection Channel per client.
- Keep client streams independent.
- Avoid persisting TCP Connection Channels.

## 155. Stream Find and Triggers

A user shall be able to:

- Define a `BytePattern` find rule and observe its matches fire: the rule's firings
  appear in the recent-matches log (snapshot `matches`, §165) and a `Mark` action
  splices its marker/timestamp inline at the matched offset (§50.2). (On-screen
  byte-range highlighting was removed in v2.0 — ADR-015.)
- Define an `Idle` rule and have it fire after the configured quiet time.
- Attach `Record` / `Notify` / `Mark` actions to a rule and observe them fire.

## 156. Display

A user shall be able to:

- View Raw, Rendered, and Hex display modes.
- View multiple display modes for one Channel.
- Configure font, foreground color, background color, wrapping, and character rendering.
- Pause and Resume display without affecting reception or recording.

Raw Display shall show actual data only and shall not insert structure of its own.

## 157. Liveness and Timing

`Listener` shall provide, per Channel:

- Total bytes received (§25)
- Rolling throughput and last-data time (§166)

Recording timestamps (§57), when enabled, use the per-chunk arrival time (§26).

## 158. Recording

A user shall be able to:

- Enable and disable recording per Channel.
- Select Raw Recording.
- Select Display Recording.
- Optionally include timestamps.

Recording shall:

- Begin only when enabled.
- Write no historical data.
- Continue during Display Pause.
- Not include metadata other than optional timestamps.

## 159. Profiles

A user shall be able to:

- Save a complete Listener workspace Profile.
- Load a Profile.
- Restore configured Channels and settings.

Profile loading shall not:

- Start Channels.
- Begin recording.
- Restore TCP Connection Channels.
- Restore runtime state.

## 160. NMEA0183

_Removed in v2.0. There is no NMEA decoder (§29); NMEA arrives as ordinary stream bytes._

---

# Part XXIX.1 — Acceptance Criteria (v1.2 addendum)

The v1.2 feature set (revision note at the top of this document) is **required**,
not optional spec detail, and carries its own acceptance bar. Each criterion below
is unverified end-to-end until proven.

## 161. Serial Control Lines

A user shall be able to: see CTS/DSR/DCD/RI state live and observe it change; set
RTS/DTR at open and toggle them live while Running. Control-line activity shall not
affect reception or Recording (§14.2–§14.3).

## 162. Auto-Reconnect

With auto-reconnect enabled on a Serial/UDP/TCP-Listener Channel, a transport fault
shall trigger automatic re-Start with backoff (`Faulted → Starting`, §9.1), bounded
by `max_attempts`. With it disabled (the default), a Faulted Channel stays Faulted.
TCP Connection Channels shall not auto-reconnect.

## 163. File Rotation

An Hourly/Daily recording shall produce period files named
`<channel>_<start-time><ext>` with the correct extension (`.raw`/`.disp`),
finalize each file cleanly at the boundary (no gap, no backfill), and reject a
filesystem-unsafe channel name at config time (§59, §71).

## 164. Subsampling

_Removed in v2.0 (§50.1). Raw recording is byte-exact-or-off; there is no message-framed `.ssdat`._

## 165. Find and Triggers

Each match condition (`BytePattern` scanned across the stream, `Idle`) shall fire
its actions; `Record` shall begin/stop from the match forward with no pre-match
backfill; `Mark` shall annotate the display / `.disp` / events but never the raw
`.raw` stream (§50.2).

## 166. Liveness

A running Channel shall surface throughput (bytes/sec), total bytes received,
time-since-last-data, and a derived idle indicator, bounded and without affecting
reception (§91.1).

## 167. Network Live Adjustment

A user shall be able to set SO_RCVBUF and join/leave multicast groups (with
interface selection) live where the OS permits; re-bind (address/port) shall occur
via apply-pending restart (§76.1, §13).

## 168. Disk-Space Guard

On low disk a recording shall warn and, if configured, stop and finalize cleanly
while reception continues; status shall surface current file, bytes written, free
space, and rotation countdown (§56.2).

---

# Appendix A — Deferred Features

Deferred in v2.1:

- Multiple simultaneous Display Views per Channel (§48 — one view with switchable
  modes ships; per-view render state would be required for independent pause)
- TCP Connection-Channel user-facing surfacing (§16.2/ADR-024): per-connection
  snapshots/stream view, per-connection recording (needs §59 filename
  templating), per-connection match rules, and `recv_buffer_bytes` on accepted
  sockets

Deferred from Version 1:

- Automatic protocol detection
- Protocol field extraction
- CSV export
- JSON export
- Structured semantic export
- Session replay
- Plugin architecture
- TCP client mode
- Distributed operation
- Advanced synchronization recovery
- Size-based file rotation (time-based rotation is supported — §59)
- Pre-trigger / pre-match recording capture (§50.2)
- Persistent diagnostic log rotation
- Hard real-time guarantees

---

# Appendix B — Example Pipelines

## B.1 Serial NMEA

```text
Serial Port
  ↓
Byte Stream (verbatim; NMEA is just bytes)
  ↓
Fan-out
  ├─ Display (Rendered shows each CRLF as a line; Hex/Raw show the bytes)
  ├─ Raw Recording (.raw)
  ├─ Display Recording (.disp)
  └─ Find / Triggers (e.g. BytePattern "$GPGGA")
```

## B.2 UDP

```text
UDP Datagram (boundary preserved)
  ↓
Byte Stream
  ↓
Fan-out (Display / Raw Recording / Display Recording / Find)
```

## B.3 TCP

```text
TCP Listener Channel
  ↓
Accept Client
  ↓
TCP Connection Channel
  ↓
Byte Stream
  ↓
Fan-out (Display / Raw Recording / Display Recording / Find)
```

---

# Appendix C — Agent Guidance

General working directives for any coding agent or human contributor are in the
workspace-level [`AGENTS.md`](../../AGENTS.md) (tool-neutral; replaces the former
Codex-specific guidance) — implement from the spec, don't invent architecture, test
before adding transports, record decisions in the ADR.

The product-scope constraints that were previously restated here are **normative in
this specification**, not agent advice, and remain in their authoritative locations:

- *Deferred features* (no automatic protocol detection, no protocol field extraction,
  no CSV/JSON export, no TCP client mode) — Appendix A.
- *Immutable received chunks, GUI out of core* — §103 and listener [`ADR.md`](ADR.md)
  ADR-010 (decoders and Messages are removed in v2.0).
- *Display formatting must not affect Raw Recording* — §5.6 / §53–§54.
- *Runtime TCP Connection Channels are not persisted* — the TCP transport sections.

