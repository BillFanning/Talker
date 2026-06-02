# Listener Specification v1.1.1

Status: Draft 1 (revised — concurrency model, recording semantics, and review fixes folded in)
Audience: human reviewers, Rust implementers, and Codex/code-generation agents
Primary implementation language: Rust
Primary editor workflow: VS Code + rust-analyzer + Codex

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

`Listener` is a cross-platform utility for receiving byte-oriented data from serial and network sources, displaying that data in multiple representations, optionally decoding protocol metadata, and optionally recording received data to files.

This document is intended to be placed at:

```text
listener/docs/listener_specification.md
```

Codex and other code-generation tools should treat this specification as authoritative unless the user explicitly revises it.

---

# Part I — Product Definition

## 1. Product Overview

`Listener` is a streamed communication data acquisition, inspection, display, and recording application.

Primary use cases:

- Inspect data arriving from serial ports.
- Inspect UDP unicast, broadcast, and multicast data.
- Listen for TCP clients and inspect their transmitted data.
- Validate NMEA0183 sentence formatting and checksum metadata.
- Display byte-oriented data in Raw, Rendered, and Hex views.
- Record original received data or rendered display output.
- Support future protocol decoders without redesigning the core pipeline.

`Listener` is a passive receive-side tool. It observes, displays, decodes, and records. It is not a packet injection tool, control system, SCADA system, or hard real-time system.

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

The UI shall not own transport handles, recording handles, decoder state, or channel pipeline state.

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

### 4.2 Byte Stream

A Byte Stream is an ordered sequence of received bytes.

Serial and TCP Connection Channels produce Byte Streams.

### 4.3 Message

A Message is a logical unit extracted from a Byte Stream or provided directly by a message-oriented transport.

Examples:

- One delimiter-terminated text record
- One fixed-length binary record
- One UDP datagram
- One NMEA0183 sentence after CRLF extraction

### 4.4 Message Extraction

Message Extraction determines where Messages begin and end.

Extraction answers:

```text
Where does this Message begin and end?
```

Extraction does not determine protocol meaning.

### 4.5 Decoder

A Decoder interprets completed Messages.

Decoding answers:

```text
What does this Message mean?
```

Decoders do not determine Message boundaries and do not modify Message contents.

### 4.6 Metadata

Metadata is supplemental information associated with a Message.

Metadata shall remain logically separate from Message contents.

### 4.7 Payload Metadata

Payload Metadata is protocol-independent Message information.

Version 1 Payload Metadata:

- Message Number
- Total Byte Count
- Arrival Timestamp
- Reception Duration

### 4.8 Protocol Metadata

Protocol Metadata is produced by Decoders.

Examples:

- Protocol Identifier
- NMEA Talker Identifier
- NMEA Sentence Identifier
- Integrity Status
- Integrity Algorithm

### 4.9 Integrity Metadata

Integrity Metadata describes validation of an integrity field.

Integrity Metadata shall distinguish between:

- Protocol integrity
- Payload integrity

Examples:

- NMEA XOR checksum = Protocol integrity
- Future payload CRC = Payload integrity

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
- Message Numbers
- Display History
- Retained Messages
- Active file handles
- Runtime warnings/errors

---

# Part III — Architectural Principles

## 5. Core Principles

### 5.1 Byte Stream First

Received bytes are the authoritative source.

Messages, metadata, decoding, display, recording, and export are derived from received bytes.

### 5.2 Extraction Defines Structure

Message Extraction determines boundaries.

Extraction shall not interpret protocol meaning.

### 5.3 Decoding Defines Meaning

Decoders interpret completed Messages.

Decoders shall not modify Messages, determine Message boundaries, or control Channel lifecycle.

### 5.4 Display Is Presentation Only

Display rendering shall not alter:

- Received bytes
- Messages
- Metadata
- Raw recordings

### 5.5 Metadata Remains Separate

Metadata shall not be inserted into, appended to, or otherwise become part of Message contents.

Metadata may be displayed adjacent to, above, below, beside, or in a dedicated metadata panel.

### 5.6 Recording Preserves Original Data

Raw Recording is the authoritative persistent representation of the received data
it contains. It is byte-exact and contiguous up to a known end, but is not
guaranteed complete under sustained overload — see fault behavior (§56.1).

Raw Recording shall preserve:

- Original byte values
- Original ordering
- Original Message ordering

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

Channel Names are user configurable and need not be unique.

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
- No message extraction
- No decoding
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
- Message extraction may occur
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

## 10. Start and Stop

Start and Stop are Channel-level commands.

### 10.1 Start

Start shall:

- Open the underlying communication interface.
- Begin receiving data.
- Initialize Message Numbering for the Channel.
- Enable configured Message Extraction.
- Enable configured Protocol Decoding.

Message Number 1 is assigned to the first completed Message after Start.

### 10.2 Stop

Stop shall:

- Close the underlying communication interface.
- Terminate data reception.
- Terminate Message Extraction.
- Terminate Protocol Decoding.
- Attempt to flush accepted recording data.

The next Start operation restarts Message Numbering at 1.

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
- Message extraction
- Decoding
- Recording
- Message Numbering
- Metadata generation

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

Message Numbering resets only because the interface was stopped and started.

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

- Message Numbering
- Message Extraction
- Recording
- Decoding

## 15. UDP Channels

UDP Channels shall support:

- Unicast
- Broadcast
- Multicast

A UDP Channel shall receive complete UDP datagrams.

Each UDP datagram shall become one Message.

UDP datagram boundaries shall be preserved.

Additional Message Extraction inside UDP payloads shall not be applied unless explicitly configured in a future version.

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
- Independent Message Numbering
- Independent Display Configuration
- Independent Recording Configuration
- Independent Metadata
- Independent runtime state

Data from different TCP clients shall not be merged into a common Byte Stream.

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

# Part VI — Message Extraction

## 17. Input Processing Model

A Channel may operate in:

- Stream Mode
- Message Mode

Users can switch between Stream Mode and Message Mode per Channel.

## 18. Stream Mode

In Stream Mode:

- Data is processed as a continuous sequence of bytes.
- No Message boundaries are recognized.
- Message Numbering is not applicable.
- Message timestamps are not applicable.

Stream Mode data is displayed as a stream and may be recorded as raw bytes.

## 19. Message Mode

In Message Mode:

- Data is buffered until Message completion criteria are satisfied.
- Completed Messages are assigned Message Numbers.
- Completed Messages may receive Arrival Timestamps.
- Completed Messages may be decoded.

## 20. Extraction Methods

Supported extraction methods:

```rust
enum ExtractionConfig {
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
```

Only one extraction method shall be active per Channel.

## 21. Delimiter-Based Extraction

A Message is complete when the configured delimiter sequence is observed.

Delimiter sequences may consist of one or more arbitrary bytes.

Examples:

```text
LF
CR
CRLF
NUL
AA 55
FF FF FF
```

Delimiter bytes may be included in or excluded from the Message according to configuration.

Delimiter detection shall support delimiter sequences that occur across receive-buffer boundaries.

## 22. Fixed-Length Extraction

A Message is complete when the configured number of bytes has been received.

Fixed-Length extraction shall support:

- Immediate Synchronization
- Optional Synchronization Marker

### 22.1 Immediate Synchronization

Message extraction begins with the first byte received after Channel Start or after extraction mode activation.

### 22.2 Optional Synchronization Marker

A synchronization marker is a user-configurable byte sequence.

When configured, Listener ignores bytes until the marker is found.

After marker detection, fixed-length extraction begins.

Version 1 shall not perform heuristic or protocol-aware synchronization recovery beyond marker detection.

## 23. Protocol-Based Extraction

Protocol-Based Extraction uses protocol-specific boundary rules.

Version 1 may use Delimiter Extraction for NMEA rather than a separate NMEA extractor.

---

# Part VII — Message Metadata

## 24. Message Number

A Message Number is a monotonically increasing integer assigned to each completed Message within a Channel.

Message numbering shall:

- Begin at 1 when a Channel is Started.
- Increase by one for each completed Message.
- Continue across configuration changes that do not Stop the Channel.
- End when the Channel is Stopped.
- Restart at 1 upon the next Start operation.
- Not be affected by retention eviction.

Message Numbers are Channel-local and not globally unique.

Message Numbers are only applicable when Message Extraction is enabled.

Listener shall not expose OS receive-block numbers or implementation receive-buffer boundaries as user-visible objects.

## 25. Total Byte Count

Total Byte Count is the number of bytes in the completed Message.

It is protocol-independent.

## 26. Arrival Timestamp

Timestamping applies only to completed Messages.

Arrival Timestamp is associated with receipt of the first byte of the Message.

Stream Mode has no Message timestamps.

Supported timestamp sources:

- Local Time
- UTC
- Relative Time Since Channel Start

Supported timestamp resolution:

- Seconds
- Milliseconds
- Microseconds

> **Precision is not accuracy.** Timestamp resolution selects how finely a time
> is displayed and stored; it does not guarantee timing accuracy. Userland
> serial/UDP arrival times carry OS scheduling and driver-buffering jitter that
> can exceed the selected resolution — a microsecond-resolution timestamp is not
> a microsecond-accurate one. See §133.

## 27. Reception Duration

Reception Duration is the elapsed time between receipt of the first byte of a Message and receipt of the final byte required to complete the Message.

Reception Duration is optional Payload Metadata.

Completion Timestamp is not a primary user-facing concept in Version 1.

## 28. Integrity Metadata

Use a unified integrity model rather than separate checksum/CRC fields.

```rust
enum IntegrityScope {
    Protocol,
    Payload,
}

enum IntegrityStatus {
    NotPresent,
    Valid,
    Invalid,
    NotChecked,
    DecoderError,
}

struct IntegrityMetadata {
    scope: IntegrityScope,
    status: IntegrityStatus,
    algorithm: Option<String>,
}
```

Examples:

```text
NMEA XOR checksum → Protocol Integrity
Future payload CRC → Payload Integrity
```

---

# Part VIII — Decoder Architecture

## 29. Decoder Responsibilities

A Decoder may:

- Parse Message contents.
- Validate protocol structure.
- Validate integrity checks.
- Produce Protocol Metadata.
- Produce Decoder Validation Errors.

A Decoder shall not:

- Modify Messages.
- Modify Byte Streams.
- Modify Recording.
- Modify Display Configuration.
- Modify Message Numbering.
- Control Channel lifecycle.

## 30. Decoder Selection

Decoder selection shall be explicit per Channel.

Automatic protocol detection is deferred from Version 1.

## 31. Decoder Output

Decoder output shall be logically separate from Message contents.

Version 1 Decoder output:

- Protocol Metadata
- Decoder Validation Errors

Version 1 shall not perform Protocol Field extraction.

Deferred examples:

- Latitude
- Longitude
- Speed
- Heading
- Device readings
- Structured semantic fields

## 32. Decoder Failure Isolation

Decoder failures shall not prevent:

- Reception
- Raw Recording
- Display of undecoded Messages
- Message Numbering

unless the Decoder is required for Message Extraction.

---

# Part IX — NMEA0183 Decoder

## 33. NMEA0183 Scope

The NMEA0183 Decoder validates and identifies NMEA0183 sentences.

It does not extract semantic fields in Version 1.

It shall use the local `nmea0183` crate for NMEA functionality.

The local `nmea0183` crate shall be standalone and suitable for independent publication.

## 34. NMEA Message Boundary Requirement

An NMEA Message must be terminated by CRLF during extraction.

Recognition requires:

- Message begins with `$` (standard/proprietary) or `!` (AIS encapsulation,
  e.g. `!AIVDM` / `!AIVDO`).
- CRLF termination was observed by the Message Extraction subsystem.

CRLF is part of extraction and need not remain in the Message payload if delimiter exclusion is configured.

## 35. NMEA Metadata

The NMEA Decoder shall produce Protocol Metadata including:

- Protocol Identifier: `NMEA0183`
- Talker Identifier
- Sentence Identifier
- Proprietary Identifier where applicable
- Integrity Status
- Integrity Algorithm: `NMEA XOR`

## 36. NMEA Checksum Validation

When a sentence contains `*hh`, the Decoder shall validate the checksum using NMEA XOR rules.

The checksum calculation shall use bytes between the start delimiter (`$` or `!`)
and `*`, excluding both delimiters.

## 37. NMEA Validation Modes

Validation mode shall be user-selectable.

```rust
enum NmeaValidationMode {
    Standard,
    Strict,
}
```

Validation mode affects only the `IntegrityStatus` assigned and the diagnostics
raised. It never suppresses display, recording, or retention: an invalid or
malformed sentence is marked and surfaced, not hidden. (Listener is a forensic
receive-side tool; bad data is often the most important data.)

### 37.1 Standard Mode

Standard Mode shall:

- Mark valid checksums `Valid`.
- Accept missing checksums, marked `NotPresent`.
- Mark malformed checksum fields `Invalid` and raise a diagnostic.

### 37.2 Strict Mode

Strict Mode shall:

- Mark valid checksums `Valid`.
- Mark missing checksums `Invalid` (Strict requires a checksum).
- Mark invalid checksums `Invalid`.
- Mark malformed checksum fields `Invalid`, each with a diagnostic.

## 38. NMEA Proprietary Sentences

The Decoder shall recognize proprietary sentences.

Examples:

```text
$PASHR
$PSRF
$PGRMZ
```

The proprietary identifier shall be exposed as Protocol Metadata.

## 39. NMEA Multi-Sentence Groups

Version 1 shall treat each NMEA sentence as an independent Message.

Version 1 shall not reassemble multi-sentence logical groups such as GSV groups
or multi-fragment AIS (`!AIVDM`) messages. Each AIS fragment is validated and
identified (talker `AI`, sentence VDM/VDO, checksum) but its armored payload is
not assembled or decoded — payload field extraction is deferred (§31).

---

# Part X — Display and Rendering

## 40. Display Architecture

Display is presentation-only.

Display functions shall not modify:

- Incoming bytes
- Messages
- Metadata
- Recorded data

## 41. Display Sources

A Display View may operate on:

- Stream data
- Completed Messages

The selected source is independent of Display Mode where practical.

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
- Not insert Message-boundary structure.

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

Grouping and spacing may be configurable.

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
- Metadata Visibility

## 48. Multi-View Display

A Channel may have multiple simultaneous Display Views.

Examples:

- Raw + Hex
- Rendered + Hex
- Raw + Rendered + Hex

Each Display View may maintain independent display settings where practical.

## 49. Metadata Display

Metadata may be displayed adjacent to or associated with Messages but shall not be inserted into Message contents.

Correct principle:

```text
Message data remains visually and logically distinct from metadata.
```

Incorrect principle:

```text
Metadata becomes part of the displayed Message text.
```

## 50. Display Pause

When Display is Paused:

- Channel operation continues.
- Message Extraction continues.
- Decoding continues.
- Recording continues if enabled.
- Retention continues according to policy.

---

# Part XI — Recording

## 51. Recording Overview

Recording is optional persistent output associated with a Channel. Recording is
not a Channel state; a Channel may be Running with recording Disabled, Enabled,
or Faulted (§12).

There are two independent recording systems, with different inputs, pipeline
positions, and guarantees:

- **Raw Recording** — byte-oriented; taps the received-chunk stream *before*
  extraction.
- **Display Recording** — rendered-output-oriented; consumes a display view's
  output *after* rendering.

Each is configured, queued, and faulted independently per Channel. Neither is an
inline pipeline stage. Both are concurrent consumers and shall never stall
reception (§100).

## 52. Recording Modes

```rust
enum RecordingMode {
    Disabled,
    Raw,
    Display,
}
```

Raw Recording is primary; Display Recording is optional. A Channel may run both
at once; each has its own state, queue, file, and fault status.

## 53. Raw Recording

Raw Recording writes received bytes exactly as received.

**Position:** it consumes the transport's `ReceivedData` chunk stream, before
Message Extraction. It therefore operates in both Stream Mode and Message Mode
and does not depend on Message boundaries.

Up to its truncation point (§56.1), Raw Recording shall preserve:

- Byte values
- Byte ordering
- Datagram / message ordering

The byte-stream artifact shall contain received payload bytes only. Optional
timestamps (§57) shall **not** be interleaved into the byte stream — doing so
would destroy byte-exactness. When enabled, timestamps are written to a separate
sidecar index keyed by byte offset.

Raw Recording is authoritative for the data it contains. It is **not** guaranteed
complete under sustained overload — see §56.1.

## 54. Display Recording

Display Recording writes the rendered output of a specific display view
(`RenderedOutput`, §141), after rendering. It is a fan-out consumer (§102),
post-render.

It may include character-rendering transformations, visible control-character
representations, and display formatting. It is explicitly not byte-exact and is
not a substitute for Raw Recording. Because the artifact is already formatted,
optional timestamps may be written inline.

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
  recorder queue uses non-blocking enqueue. Only the Transport→Extractor queue
  may exert backpressure on the reader (ADR-001, §97.1; §99).
- The recorder transitions to `Faulted`. Reception, extraction, display,
  retention, and the *other* recorder continue unaffected (§96).
- A faulted recording shall **not** silently gap and then resume. It is
  contiguous from start to a single truncation point, then ends. `Faulted` is
  terminal until the user re-enables recording, which begins a *new* artifact.
- On fault, the recorder records the truncation point — total bytes written
  (raw) or last line/message written (display), plus wall-clock and monotonic
  time — finalizes the file, emits `RecordingFaulted(ChannelId)` (§137), and
  raises a diagnostic (§101) naming the channel, time, and estimated loss where
  practical.

This is the explicit resolution of §5.6 (raw recording authoritative) versus
§100 (reception highest priority): under sustained overload Listener preserves
reception and faults the recording rather than stalling the reader or writing a
gapped file. A raw recording is therefore guaranteed **contiguous and byte-exact
for the data it contains, with a known end** — not guaranteed complete.

## 57. Timestamp Recording

Timestamp recording is optional and is the only metadata a recorder writes.
Rationale: most metadata is regenerable from recorded bytes via deterministic
extraction (§148); original timing is not.

- **Raw Recording:** timestamps are `ChunkTime` values (wall clock + monotonic)
  written to a sidecar index keyed by byte offset; the byte stream stays pure.
- **Display Recording:** timestamps may be written inline in the rendered
  artifact.

No other metadata shall be recorded by default.

## 58. Recording During Display Pause

Display Pause (§11, per display view) shall not affect either recording system.
Raw Recording is entirely upstream of display. Display Recording consumes
rendered output independently of whether a view is presented on screen; pausing a
view's presentation does not pause its recording.

## 59. Recording File Management

Recording file rotation is deferred (Appendix A).

Open questions:

- Size-based rotation
- Time-based rotation
- Filename templates
- Retention policy for recording files

---

# Part XII — Export

## 60. Export Overview

Export is an on-demand operation derived from currently retained runtime information.

Export is distinct from Recording.

## 61. Export Sources

Exports may operate on:

- Retained Messages
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
- Message Extraction Configuration
- Decoder Configuration
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
- Message Extraction configuration
- Decoder configuration
- Display configuration
- Recording configuration
- Retention configuration

## 69. Profiles Exclude

Profiles shall not contain:

- TCP Connection Channels
- Active connections
- Message Numbers
- Display History
- Retained Messages
- Runtime Metadata
- Active Recordings
- Channel Start/Stop state
- Recording Enabled/Faulted runtime state

## 70. Profile Load Behavior

Loading a Profile shall:

- Restore configured Channels.
- Restore display configuration.
- Restore recording configuration.
- Restore decoder configuration.
- Restore extraction configuration.

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
    extraction: ExtractionConfig,
    decoder: DecoderConfig,
    display: DisplayConfig,
    recording: RecordingConfig,
    retention: RetentionConfig,
}
```

### 72.1 Schema Version Compatibility

`schema_version` is a single monotonically increasing `u32`. Each binary knows
one current schema version. On load, Listener compares the profile's
`schema_version` against the version the running binary supports:

- **Equal** — load normally.
- **Profile older than the binary** — migrate forward: fill missing fields with
  `#[serde(default)]` values, log a warning, and optionally rewrite the file at
  the current version.
- **Profile newer than the binary** — refuse to load and report the version
  mismatch.

Additive fields use `#[serde(default)]` so older profiles missing newly added
optional fields load cleanly without a version bump. The version increments only
on a breaking change that defaults alone cannot absorb.

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
}
```

## 77. Decoder Configuration

```rust
enum DecoderConfig {
    None,
    Nmea0183 {
        validation_mode: NmeaValidationMode,
    },
}
```

## 78. Display Configuration

```rust
struct DisplayConfig {
    views: Vec<DisplayViewConfig>,
}

struct DisplayViewConfig {
    mode: DisplayMode,
    encoding: DisplayEncoding,
    character_rendering: CharacterRendering,
    font: Option<String>,
    foreground_color: Option<String>,
    background_color: Option<String>,
    wrapping: WrappingMode,
    metadata_visible: bool,
}
```

## 79. Recording Configuration

```rust
struct RecordingConfig {
    mode: RecordingMode,
    destination: Option<PathBuf>,
    timestamp_enabled: bool,
    overwrite_policy: OverwritePolicy,
}
```

## 80. Retention Configuration

```rust
struct RetentionConfig {
    message_limit: Option<usize>,
    byte_limit: Option<usize>,
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
pub enum ProtocolId {
    Nmea0183,
    // additional protocols may be added without a breaking change
}

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
Extraction: Stream
Decoder: None
Recording: Disabled
Display: Raw + Hex
```

## 83. NMEA Serial Template

Defaults:

```text
Kind: Serial
Name: NMEA Serial
Baud Rate: 4800
Data Bits: 8
Parity: None
Stop Bits: 1
Flow Control: None
Extraction: Delimiter CRLF
Include Delimiter: false
Decoder: NMEA0183
Validation: Standard
Recording: Disabled
Display: Raw + Metadata
```

## 84. UDP Template

Defaults:

```text
Kind: UDP
Name: UDP Channel
Bind Address: 0.0.0.0
Extraction: Datagram as Message
Decoder: None
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

- Display History
- Messages
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

- Message Count
- Byte Count
- Event Count
- Warning Count
- Error Count

Memory-based limits may be implementation-specific but are not primary user-facing limits.

## 89. Eviction

When retention limits are exceeded, oldest retained items shall be discarded first.

Message Numbers shall not be renumbered after eviction.

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
- Checksum invalid
- Display backpressure
- Unsupported decoder field
- Connection rejected (max connections reached)

## 94. Errors

Errors indicate failure to complete an operation or continue normal operation.

Examples:

- COM port not found
- COM port access denied
- TCP bind failure
- UDP bind failure
- Recording file creation failure
- Decoder initialization failure

## 95. Error Categories

```rust
enum ErrorCategory {
    Configuration,
    Resource,
    Communication,
    Recording,
    Decoder,
    Internal,
}
```

## 96. Failure Isolation

Failures shall be isolated to the smallest affected subsystem whenever practical.

Examples:

- Recording Failure faults Recording but does not stop Reception.
- Decoder Failure faults Decoder but does not stop Reception.
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

- `blocking_send` backpressure on the Transport→Extractor queue can stall the
  serial reader, which can cause UART/driver overrun. This is the mechanism
  behind "transport-specific loss" in §99 and shall be reported per §101.
- Because every stage downstream of the extractor is non-blocking (drop, evict,
  skip, or fault), the extractor never stalls on a slow consumer; the only
  condition that can stall the reader is the CPU failing to keep up with the line
  rate.
- Continuous blocking receive loops require a bounded read timeout (or handle
  close) so they can observe cancellation; shutdown does not rely on interrupting
  an in-progress blocking read (§111).
- Message timing is chunk-granular, not true per-byte hardware timing
  (`ChunkTime`, §138); per-message timing is derived in the extractor (§26, §27).

### 97.2 Blocking-to-Async Handoff

Blocking producer threads shall hand received data to the async runtime using
bounded `tokio::sync::mpsc` channels; the producer uses `Sender::blocking_send`.
This channel is the Transport→Extractor queue and participates in the §99
backpressure policy. Implementations shall not introduce an unbounded
intermediate queue between blocking transport threads and the async runtime.

## 98. Channel Isolation

Each Channel shall maintain independent:

- Reception
- Extraction
- Decoding
- Recording
- Retention
- Message Numbering

## 99. Queue and Backpressure Matrix

| Queue | Bounded | Overflow Behavior |
|---|---:|---|
| Transport → Extractor | Yes | `blocking_send`; may stall the reader (the only edge permitted to); transport-specific loss may occur and is reported (§101) |
| Chunk tap → Raw Recording | Yes | `try_send`; Raw Recording faults on full; never stalls reception |
| Extractor → Metadata | Yes | Warn; affected Channel may fault |
| Metadata → Decoder | Yes | Warn; decoder may skip messages |
| Fan-out → Display | Yes | Drop oldest display items |
| Fan-out → Display Recording | Yes | `try_send`; Display Recording faults on full; never stalls reception |
| Fan-out → Retention | Yes | Evict oldest retained items |
| Diagnostics | Yes | Drop oldest low-priority diagnostics first |

Only the Transport→Extractor edge may exert backpressure on the reader. Every
other edge shall drop, evict, skip, or fault — never stall acquisition.

### 99.1 Queue Topology

```text
transport reader
  │
  ▼
chunk distribution             ← runs on the reader thread/task
  ├─ extractor queue           [bounded; blocking_send — may stall reader]
  └─ raw recorder queue        [bounded; try_send — fault on full; never stalls]

extractor
  │
  ▼
fan-out
  ├─ display queues            [bounded; drop oldest]
  ├─ display recorder queues   [bounded; try_send — fault on full]
  ├─ retention queue           [bounded; evict oldest]
  └─ diagnostics queue         [bounded; drop oldest low-priority first]
```

Distribution order at the chunk split shall be:

1. attempt raw recorder delivery with non-blocking enqueue;
2. deliver to the extractor queue, which is the only edge permitted to block or
   stall the reader.

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

Stream-oriented Channels:

```text
Receive Bytes
  ↓
Raw Recording (optional)
  ↓
Message Extraction
  ↓
Metadata Generation
  ↓
Protocol Decoding (optional)
  ↓
Fan-out
  ├─ Display
  ├─ Recording
  ├─ Retention
  └─ Diagnostics
```

UDP Channels:

```text
Receive Datagram
  ↓
Message Creation
  ↓
Raw Recording (optional)
  ↓
Metadata Generation
  ↓
Protocol Decoding (optional)
  ↓
Fan-out
```

## 103. Ownership Principle

Each stage owns only the state it creates.

Once a Message is created, it is immutable.

Recommended sharing:

```rust
Arc<Message>
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

Extractor owns:

- Partial message buffers
- Delimiter state
- Fixed-length synchronization state

Extractor outputs completed Message bytes.

Because chunk boundaries do not align with Message boundaries (and are not a
user-visible concept — §24), the extractor associates each completed Message with
the `ChunkTime` (§138) of the chunk that supplied its first byte and the chunk
that supplied its last byte. The metadata stage uses these to compute Arrival
Timestamp (§26) and Reception Duration (§27); a Message wholly contained in one
chunk has a Reception Duration of zero (or `None`). Per-byte arrival time is not
available from the OS and is not represented.

## 106. Metadata Ownership

Metadata stage owns:

- Message counter
- Arrival timestamp calculation
- Reception duration calculation

Metadata stage creates immutable Message objects.

## 107. Decoder Ownership

Decoder consumes immutable Messages and produces Protocol Metadata.

Decoder shall not mutate Messages.

## 108. Fan-Out Ownership

Fan-out distributes immutable Messages and associated metadata to independent consumers.

No consumer owns the pipeline.

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

## 112. Partial Messages During Shutdown

Incomplete Messages at shutdown:

- Shall not receive Message Numbers.
- Shall not be decoded.
- May be discarded.
- May generate warnings if configured.

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

Messages within a Channel shall preserve receive order.

Listener shall not reorder Messages within a Channel.

If timestamps are equal, Message Number determines ordering.

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
In this workspace `nmea0183` is a **top-level sibling crate**, already shared with
`talker`; `listener` depends on it as an ordinary path/version dependency.

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
      extract/               # → listener-extract
      decode/                # → listener-decode
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
- Message model
- Metadata model
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

- NMEA
- Display
- Recording format

### listener-extract  (`src/extract/`)

Owns:

- Stream Mode
- Delimiter extraction
- Fixed-length extraction
- Sync marker handling

### listener-decode  (`src/decode/`)

Owns:

- Decoder trait
- NMEA0183 decoder adapter
- Future decoders

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

```rust
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Message {
    pub channel_id: ChannelId,
    pub number: u64,
    pub bytes: Arc<[u8]>,
    pub metadata: MessageMetadata,
}
```

Rules:

- Message bytes are immutable.
- Message Number is Channel-local.
- Message Number resets on Start.
- Message Number exists only in Message Mode.

## 132. Message Metadata

```rust
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct MessageMetadata {
    pub total_byte_count: usize,
    pub arrival_timestamp: MessageTimestamp,
    pub reception_duration: Option<Duration>,
}
```

## 133. Timestamp

Internal timing uses one model, not strings: a monotonic `Instant` for
ordering/duration and the §125 tie-break, and a wall-clock `SystemTime` for
Local/UTC display. A Message timestamp is derived from the `ChunkTime` (§138) of
the chunk containing the Message's first byte (§26). Formatting to a display
string — source (Local / UTC / Relative) and resolution (seconds / ms / µs) — is
done in the display/rendering layer, not in stored state.

```rust
#[derive(Clone, Debug)]
pub struct MessageTimestamp {
    pub monotonic: std::time::Instant,
    pub wall_clock: std::time::SystemTime,
}

// Display-time selection:
pub enum TimestampSource { Local, Utc, RelativeToStart }
pub enum TimestampResolution { Seconds, Millis, Micros }
```

Timestamp *resolution* is a display/storage precision choice, not an accuracy
guarantee; userland serial/UDP arrival times carry OS scheduling jitter that may
exceed the displayed resolution.

## 134. Protocol Metadata

```rust
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ProtocolMetadata {
    pub protocol: ProtocolId,
    pub message_type: Option<String>,
    pub integrity: Vec<IntegrityMetadata>,
    pub attributes: BTreeMap<String, String>,
}
```

For NMEA:

```text
protocol = NMEA0183
message_type = GLL
attributes:
  talker_id = GP
  proprietary_id = optional
```

## 135. Integrity Metadata

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityScope {
    Protocol,
    Payload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityStatus {
    NotPresent,
    Valid,
    Invalid,
    NotChecked,
    DecoderError,
}

#[derive(Clone, Debug)]
pub struct IntegrityMetadata {
    pub scope: IntegrityScope,
    pub status: IntegrityStatus,
    pub algorithm: Option<String>,
}
```

## 136. Runtime Commands

```rust
pub enum RuntimeCommand {
    StartChannel(ChannelId),
    StopChannel(ChannelId),
    ApplyPendingConfig(ChannelId),
    EnableRecording(ChannelId),
    DisableRecording(ChannelId),
    PauseDisplay(ChannelId, DisplayViewId),
    ResumeDisplay(ChannelId, DisplayViewId),
}
```

## 137. Runtime Events

```rust
pub enum RuntimeEvent {
    ChannelStarted(ChannelId),
    ChannelStopped(ChannelId),
    ChannelFaulted(ChannelId),
    MessageReceived(ChannelId, u64),
    RecordingFaulted(ChannelId),
    WarningRaised(ChannelId),
    TcpClientConnected(ChannelId),
    TcpClientDisconnected(ChannelId),
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
    Bytes(Vec<u8>),    // stream chunk; framing decided by the extractor
    Datagram(Vec<u8>), // already one complete message (UDP); no byte extraction
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

```rust
pub trait MessageExtractor {
    // `at` is the ChunkTime of this chunk (§138); the extractor attaches the
    // first/last chunk times to each completed Message for §26/§27.
    fn push_chunk(&mut self, bytes: &[u8], at: ChunkTime) -> Vec<MessageBytes>;
    fn finish(&mut self) -> Vec<MessageBytes>;
}
```

Extractor owns partial-message state, including the first-byte `ChunkTime` of any
Message currently being assembled.

## 140. Decoder Trait

```rust
pub trait Decoder {
    fn decode(&self, message: &Message) -> DecodeResult;
}

pub struct DecodeResult {
    pub metadata: Option<ProtocolMetadata>,
    pub errors: Vec<DecodeError>,
}
```

## 141. Renderer Trait

```rust
pub trait Renderer {
    fn render(&self, message: &Message) -> RenderedOutput;
}
```

## 142. Recorder Traits

The single `write_raw(&Message)` primitive is removed: it could not express
Stream-Mode raw recording or byte-level pre-extraction recording. It is replaced
by two writers matching the two recording systems (§51).

```rust
#[async_trait::async_trait]
pub trait RawRecorder {
    /// Append one received chunk (pre-extraction) exactly as received.
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

Recommended implementation order:

1. `listener-core`
2. `listener-extract`
3. extraction tests
4. `listener-runtime` skeleton
5. queue/backpressure tests
6. UDP transport
7. serial transport
8. TCP listener/connection transport
9. recording
10. display
11. GUI/CLI

Do not start with GUI.

---

# Part XXVIII — Testing and Verification

## 148. Testing Principle

Observable behavior matters more than implementation details.

Given identical input and configuration, Listener shall produce deterministic:

- Message boundaries
- Message ordering
- Message numbering
- Metadata
- Raw recording output

## 149. Unit Test Placement

Unit tests should live in `#[cfg(test)]` modules at the bottom of the file under test.

Integration tests shall live in the crate `tests/` directory.

## 150. Required Test Areas

Required tests:

- Delimiter extraction
- Delimiter across buffer boundary
- Consecutive delimiters
- Missing delimiter
- Fixed-length extraction
- Sync marker detection
- UDP datagram Message mapping
- Message Numbering
- Retention eviction
- Raw recording integrity
- Display Pause behavior
- NMEA checksum validation
- NMEA Standard vs Strict mode
- TCP Connection Channel creation
- State transitions
- Queue overflow handling
- Failure isolation
- Graceful shutdown

## 151. NMEA Tests

NMEA tests shall cover:

- Valid checksum
- Invalid checksum
- Missing checksum
- Malformed checksum
- Proprietary sentence
- Missing CRLF observation
- Invalid sentence identifier

## 152. Backpressure Tests

Backpressure tests shall verify:

- Display queue overflow does not stop reception.
- Recording failure faults recording but not reception.
- Retention eviction preserves Message Numbering.
- Diagnostics overflow drops low-priority entries first.

---

# Part XXIX — Acceptance Criteria

## 153. Channel Operation

A user shall be able to:

- Create Serial, UDP, and TCP Listener Channels from templates.
- Start and Stop Channels independently.
- Run multiple Channels simultaneously.
- View Channel status.

## 154. TCP Connections

A TCP Listener shall:

- Accept incoming clients.
- Create one TCP Connection Channel per client.
- Keep client streams independent.
- Avoid persisting TCP Connection Channels.

## 155. Message Processing

A user shall be able to configure:

- Stream Mode
- Delimiter Extraction
- Fixed-Length Extraction
- Optional sync marker

UDP datagrams shall become Messages.

## 156. Display

A user shall be able to:

- View Raw, Rendered, and Hex display modes.
- View multiple display modes for one Channel.
- Configure font, foreground color, background color, wrapping, and character rendering.
- Pause and Resume display without affecting reception or recording.

Raw Display shall show actual data only and shall not insert message-boundary structure.

## 157. Metadata and Timing

In Message Mode, Listener shall provide:

- Message Number
- Total Byte Count
- Arrival Timestamp
- Optional Reception Duration
- Optional Protocol Integrity Metadata

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
- Restore Message Numbers.
- Restore runtime state.

## 160. NMEA0183

The NMEA Decoder shall:

- Operate only on complete Messages.
- Require `$` (standard/proprietary) or `!` (AIS) at Message start.
- Require CRLF observation during extraction.
- Support Standard and Strict validation modes.
- Validate NMEA XOR checksums.
- Recognize proprietary sentences.
- Produce Protocol Metadata without modifying Message contents.

---

# Appendix A — Deferred Features

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
- File rotation
- Persistent diagnostic log rotation
- Hard real-time guarantees

---

# Appendix B — Example Pipelines

## B.1 Serial NMEA

```text
Serial Port
  ↓
Byte Stream
  ↓
CRLF Delimiter Extraction
  ↓
Message
  ↓
Payload Metadata
  ↓
NMEA Decoder
  ↓
Protocol Metadata
  ↓
Display / Recording / Retention
```

## B.2 UDP

```text
UDP Datagram
  ↓
Message
  ↓
Payload Metadata
  ↓
Optional Decoder
  ↓
Display / Recording / Retention
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
Message Extraction
  ↓
Pipeline
```

## B.4 Fixed-Length With Sync Marker

```text
Sync Marker: AA 55
Length: 16 bytes

Ignore bytes until AA 55 observed.
Then collect 16 bytes as one Message.
Repeat.
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
- *Read-only decoders, immutable Messages, GUI out of core* — Part on Messages/Decoders
  (§131–§135, §140) and listener [`ADR.md`](ADR.md) ADR-002.
- *Display formatting must not affect Raw Recording* — §5.6 / §1185 ff.
- *Runtime TCP Connection Channels are not persisted* — the TCP transport sections.

