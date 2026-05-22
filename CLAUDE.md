# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

```powershell
# Build the entire workspace
cargo build

# Build release
cargo build --release

# Run the binary (GUI)
cargo run -p talker -- --gui

# Run the binary (CLI)
cargo run -p talker -- --profile <name>

# Run all tests
cargo test

# Run tests for a single crate
cargo test -p nmea0183
cargo test -p talker

# Run a specific test by name
cargo test -p nmea0183 checksum::tests::xor_basic

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt

# Check formatting without writing
cargo fmt -- --check

# Verify nmea0183 optional serde feature compiles cleanly
cargo build -p nmea0183
cargo build -p nmea0183 --features serde
```

MSRV is 1.95 (current stable). Run `rustup update stable` if the build rejects your toolchain.

## Architecture

### Workspace Layout

Two crates in a Cargo workspace:

- **`nmea0183/`** — library crate; no dependency on `talker`; intended for independent crates.io publication. Handles NMEA 0183 sentence construction, parsing, checksum, talker IDs, proprietary sentences (`$PRDID`, `$PASHR`, arbitrary `$P`), and AIS sentences (`!AIVDM`/`!AIVDO` with 6-bit payload armoring).
- **`talker/`** — library plus a thin binary (ADR-014). All application logic lives in `core/`; `cli/` and `gui/` are thin interface layers that contain no business logic. `main.rs` only dispatches; `lib.rs` exports the modules so `core`'s API is unit-testable and the default dead-code lint stays active.

```
talker/src/
├── main.rs          # thin shim: dispatches to CLI or GUI based on args
├── lib.rs           # pub mod cli; pub mod core; pub mod gui;
├── cli/             # clap argument parsing; calls into core
├── gui/             # egui/eframe UI; calls into core
└── core/
    ├── channel/     # serial, UDP unicast/broadcast/multicast, TCP interfaces
    ├── message/     # payload formats, encoding, code pages, byte markers, timestamps, checksums
    ├── scheduler/   # priority-queue schedule: per-message send intervals
    ├── profile/     # TOML load/save, schema v2 (clean break — no migration)
    └── logging/     # tracing-subscriber setup; GUI status pane layer
```

### Threading Model

No async runtime. Three thread roles:

| Thread | Owns |
|--------|------|
| UI thread | egui/eframe event loop; never blocks, never does I/O |
| Talker thread (one per channel) | channel interface handle; scheduler; send loop |
| Logger thread | receives events via a crossbeam channel; writes to file and/or stdout |

`crossbeam-channel` is the only IPC mechanism. Each talker thread has a dedicated pair of crossbeam channels with the UI (commands down, status up). Interface handles are never shared across threads.

### Key Design Rules

- `core::channel` manages a **collection** of channels from day one — there is no single-channel shortcut to refactor away later.
- The UI thread never performs I/O and never blocks.
- Shared config is passed by value through channels, not via `Mutex` where avoidable.
- `nmea0183` must not import application-level crates (`anyhow`, `eframe`, `clap`, etc.).

### Error Handling

- `nmea0183` uses `thiserror` → typed `NmeaError` enum. Add `#[non_exhaustive]` before crates.io publication.
- `talker` uses `anyhow` → wrap with `.context()` for user-facing messages. Never panic in production code paths.
- The `NmeaError`→`anyhow::Error` conversion happens at the crate boundary via `?`.

### Profiles

- Format: TOML via `serde` + `toml = "1"` (OQ-2 resolved — the 1.x API is sufficient).
- Every profile struct field gets `#[serde(default)]`, so additive schema changes need no migration code.
- Header field `version: u32` — current schema is **2** (`CURRENT_VERSION` in `core::profile`). `Profile::load` refuses any profile whose version differs: a newer version is unsupported, and v1 is rejected with a "recreate the profile" error. This is a deliberate clean break — there is no `migration` module (ADR-013 update).
- An NMEA payload is stored as plain strings (`PayloadConfig::Nmea { talker, sentence_type, fields }`), not `nmea0183` types; the `nmea0183` dependency does not enable the `serde` feature (OQ-3 resolved).
- Profiles are CLI/GUI compatible. GUI-only state (window geometry, last active profile) is stored separately by `eframe`'s built-in persistence.
- Profile enums use `#[non_exhaustive]`.

### NMEA 0183 (`nmea0183` crate)

- NMEA XOR checksum is implemented inline (trivial byte fold) — no `crc` crate dependency.
- `talker` uses the `crc` crate for general checksums (CRC-8, CRC-16/CCITT, CRC-16/MODBUS, CRC-32, XOR).
- `TalkerId` and sentence type enums have a `Custom(String)` variant for non-standard IDs.
- `ProprietarySentence` has named variants (`Prdid`, `Pashr`) and a `Raw` variant. `$PRDID` does **not** include a checksum by convention. `$PASHR` field 10 (GNSS quality) is exposed as raw `u8` — Trimble and Novatel define the values differently.
- AIS sentences (`!AIVDM`/`!AIVDO`) use `!` as the start character and the `AI` talker ID. `AisSentence` builds and parses them; `armor`/`unarmor` handle the 6-bit ASCII payload encoding. AIS is a `nmea0183` library capability only — it is **not** a `talker` message format (spec §5.1 lists five formats, none of them AIS).
- Serde derives on all public types are gated behind `#[cfg_attr(feature = "serde", derive(...))]`.

### Message Formats (`core::message`)

A channel owns one or more messages; each message has a payload format, an optional prepended timestamp, and an optional appended checksum. `PayloadConfig` variants (spec §5.1):

- `RawHex` — arbitrary bytes as a hex string (spaces and hyphens stripped).
- `Utf8` / `Ascii` — text. `Ascii` carries a `CodePage` (CP437, Windows-1252, Mac OS Roman, ISO-8859-1 — hand-written tables, ADR-015).
- `Utf16` — text with a `ByteOrder` (`BigEndian` default | `LittleEndian`) and an optional BOM.
- `Nmea` — a sentence built through the `nmea0183` crate.

`Utf8`/`Ascii` text may carry non-printable bytes as inline `‹XX›` markers (U+2039, two hex digits, U+203A — spec §5.3); `core::message::marker` splits marker-aware text and `compile()` expands the markers into raw bytes. `compile()` produces the static wire bytes; the timestamp, if any, is rendered per send.

### Logging

`tracing` facade + `tracing-subscriber`. The GUI status pane is a `tracing_subscriber::Layer` that forwards events to the UI thread via `crossbeam-channel`. File log rotation uses `tracing-appender` (a workspace dependency).

### Testing Conventions

- Unit tests: `#[cfg(test)]` block at the bottom of the file under test.
- Integration tests: `talker/tests/` and `nmea0183/tests/`.
- All `core` modules need unit tests covering normal, edge, and error cases.
- `nmea0183` tests cover every sentence type: parsing, construction, and checksum.
