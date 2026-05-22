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

- **`nmea0183/`** — library crate; no dependency on `talker`; intended for independent crates.io publication. Handles NMEA 0183 sentence construction, parsing, checksum, talker IDs, and proprietary sentences (`$PRDID`, `$PASHR`, arbitrary `$P`).
- **`talker/`** — binary crate. All application logic lives in `core/`; `cli/` and `gui/` are thin interface layers that contain no business logic.

```
talker/src/
├── main.rs          # dispatches to CLI or GUI based on args
├── cli/             # clap argument parsing; calls into core
├── gui/             # egui/eframe UI; calls into core
└── core/
    ├── connection/  # serial, UDP unicast/broadcast/multicast, TCP client abstractions
    ├── data/        # BinaryField enum, encoding, file reading, data sources
    ├── scheduler/   # interval and multi-message schedule logic
    ├── profile/     # TOML load/save, schema versioning, migration
    └── logging/     # tracing-subscriber setup; GUI status pane layer
```

### Threading Model

No async runtime. Three thread roles:

| Thread | Owns |
|--------|------|
| UI thread | egui/eframe event loop; never blocks, never does I/O |
| Talker thread (one per active connection) | connection handle; scheduler; send loop |
| Logger thread | receives events via channel; writes to file and/or stdout |

`crossbeam-channel` is the only IPC mechanism. Each talker thread has a dedicated channel pair with the UI (commands down, status up). Connection handles are never shared across threads.

### Key Design Rules

- `core::connection` manages a **collection** of connections from day one — there is no single-connection shortcut to refactor away later.
- The UI thread never performs I/O and never blocks.
- Shared config is passed by value through channels, not via `Mutex` where avoidable.
- `nmea0183` must not import application-level crates (`anyhow`, `eframe`, `clap`, etc.).

### Error Handling

- `nmea0183` uses `thiserror` → typed `NmeaError` enum. Add `#[non_exhaustive]` before crates.io publication.
- `talker` uses `anyhow` → wrap with `.context()` for user-facing messages. Never panic in production code paths.
- The `NmeaError`→`anyhow::Error` conversion happens at the crate boundary via `?`.

### Profiles

- Format: TOML via `serde` + `toml = "1"`. Verify the 1.x API surface when implementing `core::profile`; fall back to `"0.8"` if needed (OQ-2).
- Every profile struct field gets `#[serde(default)]` from the first commit.
- Header field `version: u32` starts at `1`. Migration functions live in `core::profile::migration`. Refuse to load profiles with a version newer than the binary understands.
- Profiles are CLI/GUI compatible. GUI-only state (window geometry, last active profile) is stored separately by `eframe`'s built-in persistence.
- Profile enums use `#[non_exhaustive]`.

### NMEA 0183 (`nmea0183` crate)

- NMEA XOR checksum is implemented inline (trivial byte fold) — no `crc` crate dependency.
- `talker` uses the `crc` crate for general checksums (CRC-8, CRC-16/CCITT, CRC-16/MODBUS, CRC-32, XOR).
- `TalkerId` and sentence type enums have a `Custom(String)` variant for non-standard IDs.
- `ProprietarySentence` has named variants (`Prdid`, `Pashr`) and a `Raw` variant. `$PRDID` does **not** include a checksum by convention. `$PASHR` field 10 (GNSS quality) is exposed as raw `u8` — Trimble and Novatel define the values differently.
- Serde derives on all public types are gated behind `#[cfg_attr(feature = "serde", derive(...))]`.

### Binary Field Types (`core::data`)

`BinaryField` enum covers: `u8`, `u16`, `u24`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`, `RawBytes(Vec<u8>)`. Each field carries a `ByteOrder` (`BigEndian` default | `LittleEndian`). `u24` requires manual 3-byte encoding from a `u32`.

### Logging

`tracing` facade + `tracing-subscriber`. The GUI status pane is a `tracing_subscriber::Layer` that forwards events to the UI thread via `crossbeam-channel`. File log rotation requires `tracing-appender` (not yet in workspace dependencies — add it when implementing `core::logging`).

### Testing Conventions

- Unit tests: `#[cfg(test)]` block at the bottom of the file under test.
- Integration tests: `talker/tests/` and `nmea0183/tests/`.
- All `core` modules need unit tests covering normal, edge, and error cases.
- `nmea0183` tests cover every sentence type: parsing, construction, and checksum.
