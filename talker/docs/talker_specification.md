# Talker — Program Specification
**Version:** 2.1
**Language:** Rust
**Target Platforms:** Windows, macOS, Linux

Revision v2.1 (master–detail GUI, named channels, shared chrome):

- **§2.2 GUI layout** — the stacked all-channels card view is replaced by a
  **master–detail** layout: a collapsible channel list on the left (per-row status
  glyph, name, interface summary, sent count + msgs/s, last error) and a detail pane
  for the **selected** channel (header with editable name and actions, Connection
  editor, Messages editor, Output display pane). `+ Add` (per interface kind) and
  Start all / Stop all live in the list header.
- **§2.1 / §6** — the workspace has a fourth crate, `wiredata-ui` (internal shared GUI
  chrome — fonts, palette, base style; talker ADR-016). Channels gain an optional
  display `name` in the profile schema — additive `#[serde(default)]`, omitted when
  empty, so older profiles load unchanged and unnamed channels serialize as before.
  Names are cosmetic (channels stay positional); duplicates are allowed but hinted.
- **§3.2 GUI state** — window geometry is **not** persisted anymore (the window opens
  at its default size; restoring geometry after the window is shown caused a visible
  double-flash). Zoom, theme, and the last profile path still persist.
- **§2.4 / §4.3 channel transport** — the `+ Add` menu chooses a channel's transport
  from the same UDP / TCP / Serial templates as listener. The transport is structural;
  the detail pane configures that transport's parameters but does not change its kind.

Revision v2.0.1 (corrections to match the implementation and its dependencies):

- **§4.2 serial parameters** — parity is `None, Even, Odd` and stop bits are `1, 2`.
  The `serialport` backend offers no Mark/Space parity and no 1.5 stop bits; the
  earlier list over-promised the dependency (neither ever worked). Corrected, not
  removed behavior.
- **§4.1 UDP Multicast** — now states the full configurable set: group address,
  port, outgoing interface, and TTL (all implemented).
- **§8.1 scheduling** — clarified that the "priority-queue scheduler" is a
  conceptual model, implemented as a linear next-fire scan (equivalent at the
  message counts talker handles).
- **§3.1 CLI** — single-channel ad-hoc invocation without a profile is marked as
  planned; the current CLI loads a profile.

No `schema_version` change: profiles are unaffected (the multicast `interface`/`ttl`
keys are additive `#[serde(default)]` fields — older profiles load unchanged).

---

## 1. Purpose and Goals

`talker` is a production-quality utility for sending byte-oriented data from a host computer to external devices via serial (RS-232) or network connections. Its primary use case is testing and validating receiving devices. It is designed for a small technical team with the intention of broader distribution if the tool proves successful.

---

## 2. Architecture

### 2.1 Crate Structure

`wiredata` is a Cargo workspace with four crates:

- **`talker`** — the binary crate containing the CLI, GUI, and all application logic
- **`nmea0183`** — a standalone library crate containing all NMEA 0183 support, with no dependency on `talker`
- **`listener`** — the receive/decode counterpart to `talker`, with its own spec, ADR, and TODO
- **`wiredata-ui`** — internal shared GUI chrome for the two apps (fonts, color palette, base widget style, formatting helpers); egui-only, never published (talker ADR-016 / listener ADR-019)

Each crate keeps its own `docs/` folder (spec, ADR, TODO). There is no
workspace-root docs directory.

```
wiredata/                        # workspace root
├── Cargo.toml                   # workspace manifest
│
├── talker/                      # binary crate
│   ├── Cargo.toml
│   ├── docs/
│   │   ├── ADR.md              # workspace- and talker-level decisions
│   │   ├── TODO.md
│   │   └── talker_specification.md
│   ├── src/
│   │   ├── main.rs              # entry point; dispatches to CLI or GUI
│   │   ├── cli/                 # CLI interface module
│   │   ├── gui/                 # egui GUI module
│   │   └── core/                # all core logic
│   │       ├── channel/         # serial, UDP, TCP abstractions; channel collection
│   │       ├── message/         # message formats, encoding, timestamp, checksum
│   │       ├── scheduler/       # priority-queue send loop
│   │       ├── profile/         # profile management; schema v2 clean break
│   │       └── logging/         # logging subsystem
│   └── tests/                   # integration tests (Rust convention)
│
├── nmea0183/                    # library crate (publishable independently)
│   ├── Cargo.toml
│   ├── docs/
│   │   ├── ADR.md              # nmea0183-specific decisions (ADR-009, OQ-4)
│   │   ├── TODO.md
│   │   └── nmea0183_specification.md
│   ├── src/
│   │   ├── lib.rs               # public API surface
│   │   ├── sentence/            # sentence types, construction, parsing
│   │   ├── talker_id.rs         # talker ID enum and custom variant
│   │   ├── checksum.rs          # XOR checksum computation and verification
│   │   └── proprietary/         # $PRDID, $PASHR, and arbitrary $P builder
│   └── tests/                   # integration tests for nmea0183
│
└── listener/                    # receive/decode utility crate
    ├── Cargo.toml
    ├── docs/
    │   ├── ADR.md              # listener decisions (own ADR numbering)
    │   ├── TODO.md
    │   └── listener_specification.md
    └── src/
```

### 2.2 Interface Separation

The CLI and GUI are thin interface layers only. All logic — channel management, message construction, scheduling, encoding, profile handling, logging — lives in `core`. Neither interface layer contains business logic.

### 2.3 GUI Framework

The GUI is built with **egui** (via `eframe`). The aesthetic is utilitarian and functional; not a consumer-style app. Clarity and density of information are prioritized over visual polish.

### 2.4 Multithreading Architecture

`talker` uses OS threads (`std::thread`) with `crossbeam` channels for inter-thread communication. Tokio or other async runtimes are not used; the workload (a bounded number of channels, synchronous serial I/O, a single egui UI thread) does not benefit from async and would be complicated by it. Note that "no async runtime" and "multiple OS threads" are independent concepts — `talker` uses multiple OS threads throughout.

#### Multi-Channel as a First-Class Design Goal

Sending simultaneously on multiple channels is a primary use case, not a future option. A user may need to simultaneously drive a GPS simulator on a serial port, a depth sounder on UDP, and an ADCP on a second serial port — all from one `talker` instance with one GUI.

The correct model is one application instance, one GUI, and one talker thread per active channel. `core::channel` manages a *collection* of channels from the initial implementation.

#### Thread Model

| Thread | Responsibility |
|--------|----------------|
| **UI thread** | Runs the egui/eframe event loop; handles all user interaction; never blocks |
| **Talker thread (one per active channel)** | Runs the priority-queue scheduler for that channel; owns the interface handle; handles open/close/reopen |
| **Logger thread** | Receives log messages via channel; writes to file and/or stdout without blocking talker threads |

#### GUI Multi-Channel Layout (master–detail)

The GUI uses a **master–detail** layout:

- **Channel list** (left, resizable, collapsible to a thin status strip): one row per
  channel showing a status glyph (running / fault / stopped), the channel's display
  name (or the positional "Channel N" fallback), a one-line interface summary with
  unfilled fields flagged, the running send count with a msgs/s estimate, and the most
  recent error. The list header holds `+ Add` (per interface kind: UDP / TCP / Serial)
  and Start all / Stop all.
- **Detail pane** (centre): everything about the **selected** channel — a header with
  the editable name, interface summary, drift badge, and per-channel actions (start /
  stop / restart / remove); the Connection editor for the transport selected at
  creation; the Messages editor (one or more messages, each independently configured);
  and the real-time outbound display pane (configurable view — see Section 5.7).

Channels can be added, renamed, reconfigured, removed, started, and stopped
independently at runtime. `+ Add` chooses a channel's transport kind; normal
configuration edits only that kind's parameters, matching listener's model. Channel
names are cosmetic (channels are positional); empty names fall back to "Channel N",
and duplicate names are allowed but flagged inline.

#### Communication

All cross-thread communication uses `crossbeam` channels. Each talker thread has its own dedicated channel pair with the UI:

- **UI → Talker (per channel):** commands (start, stop, parameter change, profile load, message interval update)
- **Talker → UI (per channel):** status updates (channel state, bytes sent, errors, display data)
- **Any thread → Logger:** log messages (level + text)

#### Design Rules

- The UI thread never performs I/O and never blocks.
- Each talker thread exclusively owns its interface handle; handles are never shared across threads.
- Shared configuration is passed by value through channels, not via shared memory or `Mutex` where avoidable.
- The number of simultaneous channels is bounded by available system resources (serial ports, network sockets), not by any artificial limit in the software.

---

## 3. Interfaces

### 3.1 CLI

The CLI uses structured argument parsing (`clap`). All features available in the GUI are also available from the CLI, except window management, layout state, and the data display pane (which are inherently GUI concepts). This includes:

- Selecting channel type and parameters
- Selecting message format, encoding, and data
- Loading profiles (single or multi-channel)
- Enabling stdout echo
- Selecting log destinations (stdout, file, or both)
- Real-time parameter adjustment is not applicable in CLI mode; parameters are set at launch

A `--gui` flag launches the GUI from the CLI.

#### Multi-Channel in CLI Mode

CLI mode supports multiple simultaneous channels identically to GUI mode — one `talker` process, one talker thread per channel, all running in parallel. The primary way to launch multiple channels from the CLI is via a multi-channel profile:

```
talker --profile full_bridge_sim
```

This loads the profile, spawns a talker thread for each channel defined in it, and runs until interrupted. *(Planned)* Single-channel ad-hoc invocation without a profile — a fast path for simple cases — is not yet implemented; today the CLI requires `--profile` / `--profile-path`.

#### Profile Compatibility

Profiles are fully compatible between CLI and GUI. A profile saved from the GUI loads correctly in the CLI and vice versa. A profile written by hand in a text editor (valid TOML matching the profile schema) works in both.

GUI-only settings (window geometry, panel layout, display column toggles, last active profile name) are stored in a separate GUI state file and are not part of a profile.

**Example invocation sketch** (illustrative, not final):
```
talker --profile gps_sim
talker --profile full_bridge_sim
talker --gui
talker --channel serial --port COM3 --baud 9600 --format nmea0183
```

### 3.2 GUI

The GUI is built with egui/eframe and supports:

- Standard window operations: resize, minimize, maximize
- All core functionality exposed via controls
- Real-time status display (channel state, data sent, errors)
- Profile save/load/switch

#### GUI State Persistence

GUI state and profile data are stored separately and serve different purposes:

- **Profiles** (channel config, message config, checksum settings) are TOML files shared between CLI and GUI. They live in the profile directory and are the primary unit of saved work.
- **GUI state** (window size, position, which panels are open, display column toggles, name of the last active profile) is stored in a separate file using `eframe`'s built-in persistence mechanism. It is never loaded by the CLI and never conflicts with profile data.

On exit, the GUI saves its state automatically. On next launch, the GUI reopens the last active profile and restores zoom and theme. Window geometry is deliberately **not** persisted: eframe restores it only after the window is first shown, which produced a visible double frame/title-bar flash on every launch (and could resurrect a broken tiny geometry) — the window always opens at its default size instead.

GUI state is stored in a platform-appropriate config directory (via the `dirs` crate or `eframe`'s default storage path).

---

## 4. Channels

### 4.1 Supported Interface Types

Each channel has exactly one interface port. The supported interface types are:

| Type | Notes |
|------|-------|
| RS-232 Serial | Full parameter configuration (see 4.2) |
| UDP Unicast | Host and port configurable |
| UDP Broadcast | Broadcast address and port configurable |
| UDP Multicast | Group address, port, outgoing interface, and TTL configurable |
| TCP Client | Connect to a remote host/port |

The channel abstraction in `core::channel` is designed for easy addition of future interface types (e.g., WebSocket, raw socket) without changes to the interface layers or scheduler.

### 4.2 Serial Configuration

All standard RS-232 parameters are user-configurable:

- Port (e.g., COM3, /dev/ttyUSB0)
- Baud rate: common standard rates are selectable from a list (110, 300, 1200, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600); a free-entry field allows specifying any rate outside this range
- Data bits (5, 6, 7, 8)
- Parity (None, Even, Odd)
- Stop bits (1, 2)
- Hardware flow control (RTS/CTS, None)

### 4.3 Real-Time Parameter Changes

Interface parameters (port, baud rate, UDP port, host address, etc.) are adjustable while the program is running in GUI mode. When a change requires closing and reopening the interface:

- Output pauses briefly
- The user is notified visibly in the GUI (status indicator)
- The interface is automatically closed, reconfigured, and reopened
- Output resumes without user action

The transport kind itself is selected when the channel is created and is not an
in-place interface parameter. A different transport is represented by a newly added
channel, as in listener.

### 4.4 Per-Channel Monitoring

Each channel maintains and displays:

- **Send count** — total messages successfully sent since the channel was started
- **Error count** — total send errors since the channel was started
- **Status indicator** — current state: idle, running, error
- **Error log** — the most recent error is always visible inline; clicking it opens a scrolling list of all errors for that channel session

---

## 5. Messages

Each channel has one or more messages. Messages within a channel share the same interface port but are otherwise independent — each has its own format, encoding, payload, timing, timestamp configuration, and checksum configuration.

### 5.1 Message Formats

| Format | Description |
|--------|-------------|
| Hex | Arbitrary byte sequence entered as hexadecimal pairs; spaces and hyphens allowed as separators |
| UTF-8 | Unicode text encoded as UTF-8 |
| UTF-16 | Unicode text encoded as UTF-16; byte order (LE or BE) and optional BOM are user-configurable |
| ASCII | Text restricted to the ASCII range or a selected extended code page (see 5.2) |
| NMEA 0183 | Handled by the `nmea0183` crate (see Section 6) |

All format selections are saved as part of a profile.

### 5.2 ASCII Code Pages

When the message format is ASCII, the user selects a code page. This determines how characters with values 128–255 are encoded in the outgoing byte stream. The supported code pages are:

| Code Page | Common Use |
|-----------|-----------|
| CP437 | IBM PC / DOS (original IBM PC character set) |
| Windows-1252 | Windows Western European (ANSI) |
| Mac OS Roman | Classic Mac OS Western European |
| ISO-8859-1 | Latin-1; standard on Linux/Unix systems |

All four code pages are available regardless of the host operating system. This allows `talker` running on any platform to generate byte streams matching the expectations of a device or system built for a specific OS.

### 5.3 Character Entry

For ASCII and UTF-8 messages, the user types printable characters directly into a text field.

**Non-printable and extended characters** are entered using an **Insert Byte** dialog: a button opens a small dialog where the user enters a single byte value as two hex digits. The inserted byte is shown inline in the text field as a marker (e.g., `‹1B›` for ESC, `‹0D›` for CR). The marker is rendered distinctly from surrounding printable text so the message structure is clear at a glance.

For messages that are predominantly binary (many non-printable bytes), the **Hex format** is the appropriate choice and provides a more efficient entry experience than the Insert Byte dialog.

### 5.4 Timestamp

Each message optionally prepends an ISO 8601 timestamp to the wire output. The timestamp is generated at the moment the message is sent.

Timestamp configuration is per-message and includes independently toggleable components:

- **Date** (YYYY-MM-DD) — include or exclude
- **Time-of-day** (HH:MM:SS) — always included when the timestamp is enabled
- **Milliseconds** (.mmm) — include or exclude
- **Timezone designation** (e.g., `Z` for UTC) — include or exclude

Example forms (all representing the same moment):
```
2026-05-21T14:30:45.123Z     (date + time + ms + timezone)
2026-05-21T14:30:45Z         (date + time + timezone, no ms)
14:30:45.123                 (time + ms only, no date or timezone)
14:30:45                     (time only)
```

The timestamp is prepended to the payload. The outer checksum (Section 5.5), if enabled, covers the complete wire output including the prepended timestamp.

### 5.5 Checksum

Each message optionally appends a checksum to the complete wire output. This checksum is computed over the entire outgoing byte sequence — including the prepended timestamp if present — and appended after the payload.

This outer checksum is independent of and does not replace any checksum that is part of the message protocol itself. For example, an NMEA 0183 sentence includes its own `*XX` checksum inside the payload; the outer checksum wraps the entire output including that internal checksum.

Wire format when both timestamp and outer checksum are enabled:

```
[timestamp][payload][outer_checksum]
```

Checksum configuration is per-message and includes:

- **Algorithm** — selected from the supported list (see Section 7)
- **Intentionally wrong checksum** — option to send a deliberately incorrect value for negative testing

Checksum configuration is saved as part of a profile.

### 5.6 Send Interval

Each message has a send interval in milliseconds. An interval of zero means the message is dormant and is excluded from the send queue. There is no upper bound on the interval.

When an interval is changed while the channel is running:

- **Changed to zero** — the message is removed from the send queue immediately; other messages are unaffected; the channel continues running
- **Changed to a non-zero value** — the message is removed from the queue and re-inserted with its next fire time set to `now + new_interval`; other messages are unaffected

### 5.7 Data Display Pane

Each channel includes a real-time display pane showing outgoing data as it is sent. The view is configurable between:

- **Hex** — each byte shown as two uppercase hex digits (e.g., `0D 0A`)
- **ASCII** — printable characters shown as-is; control characters rendered as replacement symbols
- **Decoded text** — valid UTF-8 sequences rendered as Unicode characters; invalid bytes shown as `U+FFFD`

The display mode is a GUI-only setting and is not saved in the profile.

#### Control Character Rendering

In ASCII display mode, control characters (bytes 0x00–0x1F and 0x7F) are rendered as visible symbols in one of three user-selectable styles:

| Style | Name | Example (LF / CR / ESC) |
|-------|------|--------------------------|
| A | Unicode control pictures (U+2400 block) | `␊` / `␍` / `␛` |
| B | Bracketed abbreviations | `[LF]` / `[CR]` / `[ESC]` |
| C | Hex escape codes | `<0x0A>` / `<0x0D>` / `<0x1B>` |

Style A is the default. All styles cover the full C0 range (0x00–0x1F) and DEL (0x7F).

Display panes are available in the GUI only. In CLI mode, stdout echo (Section 5.8) serves as the data visibility mechanism.

### 5.8 Standard Output Echo

Output data is optionally echoed to stdout. This is toggled by the user and is available in both CLI and GUI modes.

---

## 6. NMEA 0183 — `nmea0183` Crate

The `nmea0183` library crate handles all NMEA 0183 formatted data. It is:

- A standalone library crate at `nmea0183/` in the workspace
- Written with no dependencies on the `talker` crate
- Designed to be published to crates.io independently for use in other Rust projects

The crate includes:

- Sentence parsing (input validation and field extraction)
- Sentence construction (building valid sentences with correct checksum)
- Sentence type identification
- Checksum computation (XOR of all bytes between `$` and `*`) and verification
- Talker ID selection and validation
- Arbitrary/proprietary sentence construction (see 6.3)

---

### 6.1 Talker IDs

All standard NMEA 0183 talker identifiers are supported as selectable options. The user selects the talker ID when constructing a sentence; any valid two-character ID is accepted. The following are the defined standard talker IDs:

| ID | Device / System |
|----|-----------------|
| AG | Autopilot — General |
| AP | Autopilot — Magnetic |
| CD | Communications — Digital Selective Calling (DSC) |
| CR | Communications — Receiver / Beacon Receiver |
| CS | Communications — Satellite |
| CT | Communications — Radio-Telephone (MF/HF) |
| CV | Communications — Radio-Telephone (VHF) |
| CX | Communications — Scanning Receiver |
| DF | Direction Finder |
| EC | Electronic Chart Display & Information System (ECDIS) |
| EP | Emergency Position Indicating Beacon (EPIRB) |
| ER | Engine Room Monitoring Systems |
| GA | Galileo receiver |
| GB | BeiDou (BDS) receiver |
| GL | GLONASS receiver |
| GN | Combined / multi-constellation GNSS |
| GP | Global Positioning System (GPS) |
| GQ | QZSS receiver |
| HC | Heading — Magnetic Compass |
| HE | Heading — North Seeking Gyro |
| HN | Heading — Non North Seeking Gyro |
| II | Integrated Instrumentation |
| IN | Integrated Navigation |
| LC | Loran-C receiver |
| RA | RADAR / ARPA |
| SD | Sounder — Depth |
| SN | Electronic Positioning System, other/general |
| SS | Sounder — Scanning |
| TI | Turn Rate Indicator |
| VD | Velocity Sensor — Doppler |
| VW | Velocity Sensor — Speed Log, Water, Mechanical |
| WI | Weather Instruments |
| YX | Transducer |
| ZA | Timekeeper — Atomic Clock |
| ZC | Timekeeper — Chronometer |
| ZQ | Timekeeper — Quartz |
| ZV | Timekeeper — Radio Update (WWV/WWVH) |

In addition to these, the user may enter any arbitrary two-character talker ID to accommodate non-standard or future devices.

---

### 6.2 Supported Sentence Types

The following sentence types are supported for construction and parsing. Any talker ID from Section 6.1 may be combined with any applicable sentence type.

| Category | Sentences | Description |
|----------|-----------|-------------|
| GNSS / Position | GGA | GPS fix data — position, time, quality |
| | GLL | Geographic position — latitude/longitude |
| | GNS | Fix data — multi-constellation |
| | RMA | Recommended minimum — Loran-C data |
| | RMB | Recommended minimum — navigation data |
| | RMC | Recommended minimum — specific GPS/transit |
| | VTG | Track made good and ground speed |
| | ZDA | Date and time (UTC + local zone) |
| Satellite Data | GSA | DOP and active satellites |
| | GSV | Satellites in view |
| Heading | HDG | Heading — magnetic, deviation, variation |
| | HDM | Heading — magnetic |
| | HDT | Heading — true |
| Speed / Water Log | VHW | Water speed and heading |
| | VBW | Dual ground/water speed |
| | VLW | Distance traveled through water |
| Depth | DBT | Depth below transducer |
| | DPT | Depth of water (with keel offset) |
| Wind | MWD | Wind direction and speed — true |
| | MWV | Wind speed and angle — apparent or true |
| Water Temperature | MTW | Mean temperature of water |
| Navigation / Autopilot | APB | Autopilot sentence B |
| | BOD | Bearing — origin to destination |
| | XTE | Cross-track error |
| Set and Drift | VDR | Set and drift |
| Transducer / Environment | XDR | Transducer measurement (generic — pressure, temp, humidity, etc.) |
| AIS | AIVDM | AIS VHF data-link message (received from other vessels) |
| | AIVDO | AIS VHF data-link own-vessel report |

Note: AIS sentences use `!` as the start character and the `AI` talker ID regardless of source. The 6-bit ASCII payload armoring is handled by the module.

---

### 6.3 Proprietary Sentence Support

NMEA 0183 proprietary sentences begin with `$P` followed by a manufacturer code and data fields. The `nmea0183` crate supports these in two ways:

**Named proprietary sentences** — fully implemented with field-by-field construction and validation for known formats:

| Sentence | Manufacturer / Origin | Fields |
|----------|----------------------|--------|
| `$PRDID` | Teledyne RDI (ADCP) | Pitch (sddd.dd), Roll (sddd.dd), Heading true (ddd.dd) |
| `$PASHR` | Ashtech / RT300; also output by OxTS, Applanix, SBG, Trimble, Novatel | UTC time, Heading true (hhh.hh), T flag, Roll (rrr.rr), Pitch (ppp.pp), Heave (xxx.xx), Roll accuracy (a.aaa), Pitch accuracy (b.bbb), Heading accuracy (c.ccc), Aiding status, IMU status |

`$PRDID` does not include a checksum by convention. `$PASHR` includes a standard NMEA checksum. The `$PASHR` GNSS quality field (field 10) is exposed as a raw `u8` because Trimble and Novatel define the values differently.

**Arbitrary proprietary sentence builder** — for any `$P` sentence not in the named list, the user can enter the manufacturer code, data fields, and opt to append a standard NMEA checksum or omit it.

Additional named proprietary sentences may be added as requirements are identified.

---

## 7. Checksum and CRC Support

Checksums are configured per message (see Section 5.5). They are optional; the default is no checksum. This is entirely separate from the NMEA protocol checksum, which is handled by the `nmea0183` crate.

### 7.1 Supported Algorithms

At minimum the following are supported:

| Algorithm | Common uses |
|-----------|-------------|
| XOR (1-byte) | NMEA-style, simple device protocols |
| CRC-8 | Sensor buses, simple embedded protocols |
| CRC-16/CCITT | Serial protocols |
| CRC-16/MODBUS | MODBUS RTU |
| CRC-32 | File integrity, Ethernet |

Additional algorithms may be added as specific device requirements are identified.

### 7.2 Behavior

The checksum is computed over the complete wire output for the message — including the prepended timestamp if present. The result is appended after the payload. The option to intentionally send an incorrect checksum (for negative testing) is supported.

Checksum configuration is saved as part of a profile.

---

## 8. Scheduling and Profiles

### 8.1 Scheduling

Each channel runs a **priority-queue scheduler**. Each message within the channel has its own independent send interval and is scheduled independently of all other messages. ("Priority queue" describes the model; the implementation is a linear next-fire scan over the channel's messages, which is equivalent and faster at the small message counts talker handles.)

**Queue model:**

- The scheduler maintains a priority queue sorted by next-fire-time.
- When a channel starts, all enabled messages (interval > 0) are inserted into the queue with next-fire-time = now (all fire immediately at t=0).
- The scheduler picks the message with the earliest next-fire-time, waits until that time, sends the message, then re-inserts it with next-fire-time = previous-fire-time + interval.
- When two messages are due at the same time, they fire in list order (the order in which they appear in the message list for that channel).
- **After a stall** (a blocked send, the machine sleeping), a message that is more than one interval overdue fires **once**, then its next-fire-time jumps forward to the first grid point (`previous-fire-time + k·interval`) still in the future. Missed intervals are skipped, not burst out back-to-back: talker generates test cadence, so a receiver should see the rate resume, not a flood catching up the count.

**Interval = 0 (dormant):** A message with interval = 0 is excluded from the queue and does not send. Changing a message's interval to 0 while the channel is running removes it from the queue immediately; other messages are unaffected and the channel continues running.

**Live interval changes:** When a message's interval is changed to a non-zero value while the channel is running, the message is removed from the queue and re-inserted with next-fire-time = now + new-interval. All other messages continue unaffected. The channel is never stopped by an interval change.

### 8.2 Profiles

A profile is a named, saved configuration. A profile defines one or more channels, each with its full configuration. This makes multi-channel operation a natural part of the profile system.

Each channel entry within a profile includes:

- Interface type and all parameters
- One or more message definitions, each containing:
  - Format and encoding (including code page for ASCII)
  - Payload data
  - Timestamp configuration
  - Checksum configuration
  - Send interval

Profiles can be:

- Named, saved, loaded, and deleted
- Switched at runtime in the GUI
- Specified by name at launch in the CLI (`--profile <name>`)
- Stored in TOML so they can be inspected, edited, and version-controlled outside the program

**Example profile sketch** (illustrative, not final schema):
```toml
version = 2
name = "GPS sim"

[[channels]]
name = "GPS feed"   # optional display name (v2.1); omitted when unnamed
type = "serial"
port = "COM3"
baud = 9600

  [[channels.messages]]
  format = "nmea0183"
  talker = "GP"
  sentence = "GGA"
  fields = ["143045.00", "4807.038", "N", "01131.000", "E"]
  interval_ms = 1000

  [channels.messages.timestamp]
  enabled = true
  include_date = false
  include_ms = true
  include_timezone = false

  [channels.messages.checksum]
  enabled = false

  [[channels.messages]]
  format = "hex"
  data = "DEAD BEEF"
  interval_ms = 500

  [channels.messages.timestamp]
  enabled = false

  [channels.messages.checksum]
  enabled = true
  algorithm = "crc16_ccitt"
```

#### Profile Schema Versioning

Every profile file includes a `version` integer field in its header. The current schema version is `2`. This field enables `talker` to detect and handle schema changes as the program evolves.

**Loading behavior by version:**

- **Matches current version:** load normally.
- **Older version:** refuse to load and instruct the user to recreate the profile. Version 1 predates release, so the v2 schema deliberately takes a clean break rather than carrying migration code.
- **Newer version than the running binary understands:** warn the user and refuse to load.

All profile fields use `#[serde(default)]` so that additive changes within schema version 2 can load cleanly when fields are missing. The version number only increments when a breaking schema change occurs that `serde(default)` cannot handle alone; a future breaking version can add migration code at that point.

---

## 9. Error Handling and Logging

### 9.1 Error Handling

All errors are handled explicitly. The program uses Rust's `Result` type throughout and does not panic in production code paths. Errors are surfaced to the user with clear, actionable messages.

### 9.2 Logging

Logging uses the `tracing` facade with `tracing-subscriber` for dispatch. Log levels are consistent across both interfaces:

| Level | Examples |
|-------|---------|
| ERROR | Connection failure, file not found, encoding error |
| WARN | Parameter change caused reconnect, malformed record skipped |
| INFO | Channel opened/closed, profile loaded, send started/stopped |

#### CLI Logging

In CLI mode, log output destinations are independently selectable at launch:

- **stdout** — enabled or disabled via flag
- **log file** — enabled or disabled via flag; path is configurable or defaults to a platform-appropriate location

#### GUI Logging

The GUI includes a **status pane** that displays errors, warnings, and info messages in real time. The status pane is always present and cannot be disabled.

Per-channel errors also appear in the channel's own error log (Section 4.4) in addition to the global status pane.

File logging in the GUI is optional and toggled by the user. Log file rotation limits total disk usage.

---

## 10. Testing

Testing follows Rust conventions:

- **Unit tests** live in `#[cfg(test)]` modules at the bottom of the file they test.
- **Integration tests** live in the `/tests` directory of each crate.

Coverage requirements:

- All `core` modules have unit tests covering normal operation, edge cases, and error conditions.
- The `nmea0183` crate has thorough unit tests covering parsing, construction, and checksum logic for all supported sentence types.
- Integration tests in `talker/tests/` cover end-to-end flows: profile load → channel open → scheduler run → data send.
- Tests must pass on all three target platforms in CI.

---

## 11. Code Quality

- Production-level code throughout; no prototype or placeholder logic in the final deliverable.
- All public APIs are documented with doc comments (`///`).
- `clippy` warnings are resolved; `rustfmt` formatting is enforced.
- Dependencies are chosen conservatively; each must be justified.

---

## 12. Open Items and Planned Future Features

### 12.1 Open Items

- Additional interface types beyond TCP/UDP/serial (WebSocket, raw socket, etc.)
- Additional CRC/checksum algorithms beyond the initial set
- Additional named proprietary NMEA sentences beyond `$PRDID` and `$PASHR`
- Installer/packaging requirements for broader distribution
- Binary field construction (typed fields: u8, u16, u24, u32, u64, i8, i16, i32, i64, f32, f64 with per-field byte order) — deferred; hex format covers the immediate need

### 12.2 Planned Future Features

- **File source** — send data read from a file, either as a raw byte stream or as parsed records (one per send interval)
- **Keyboard injection** — real-time injection of data into an active channel from the keyboard (GUI only)
- **Capture and replay** — record live output to a file, then replay it exactly including original timing
- **Auto-response / triggers** — monitor incoming data and automatically send a configured response when a specific byte pattern is detected
- **Ad-hoc multi-channel CLI flags** — repeated `--channel` flags for launching multiple channels without a profile file

---
