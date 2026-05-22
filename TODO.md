# TODO
**Version:** 1.0

Implementation reminders — small concrete tasks that need to happen during normal development. Not architectural decisions (those go in the ADR's Open questions section).

Cross off items as they are completed. Add new ones inline as they come up.

---

## Before publishing `nmea0183` to crates.io

- [ ] Add publication metadata to `nmea0183/Cargo.toml`:
  - `repository = "..."`
  - `documentation = "..."` (or rely on docs.rs default)
  - `readme = "README.md"`
  - `keywords = ["nmea", "nmea0183", "marine", "gnss", "gps"]` (max 5)
  - `categories = ["parser-implementations", "encoding"]` (must match crates.io category slugs)
- [ ] Write `nmea0183/README.md`.
- [ ] Add `#[non_exhaustive]` to `NmeaError` (per ADR-004) and to public enums per ADR-013.
- [ ] Resolve OQ-4 (library MSRV policy) in a new ADR.

## When writing `nmea0183` source

- [ ] Gate serde derives on all public types behind `#[cfg(feature = "serde")]`. The feature exists in `Cargo.toml` but the code must opt in to it.
  - Example: `#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]`
- [ ] Verify the optional-serde feature actually compiles cleanly with `cargo build` (default) and `cargo build --features serde`.

## When writing `core::logging`

- [ ] Add `tracing-appender` (or equivalent) to workspace dependencies for rotating file output. Spec §9.2 requires log file rotation; `tracing-subscriber` alone does not provide it.
- [ ] Implement the dual-mode subscriber: CLI writes to stdout/file; GUI captures events into a `tracing_subscriber::Layer` that forwards to the UI thread via `crossbeam-channel` (per ADR-006 consequences).

## When writing `core::profile`

- [ ] Resolve OQ-2: verify `toml = "1"` API matches needs, or pin to `"0.8"`.
- [ ] Resolve OQ-3: decide whether profiles serialize `nmea0183` types directly (requires enabling the `serde` feature on the `nmea0183` dependency in `talker/Cargo.toml`) or via a `talker`-side representation.
- [ ] Implement `PROFILE_SCHEMA_VERSION: u32` constant and version check on every load (per ADR-013).
- [ ] Apply `#[serde(default)]` to all profile fields from the first commit (per ADR-013).
- [ ] Apply `#[non_exhaustive]` to profile enums (per ADR-013).

## When writing the README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.

## Pre-publication of `nmea0183` (workspace change)

- [ ] Update `talker/Cargo.toml`: `nmea0183 = { path = "../nmea0183", version = "0.1" }` (per OQ-1).
