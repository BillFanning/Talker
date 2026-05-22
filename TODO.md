# TODO
**Version:** 1.1

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
- [ ] Resolve OQ-4 (library MSRV policy) in a new ADR.
- [ ] Update `talker/Cargo.toml`: `nmea0183 = { path = "../nmea0183", version = "0.1" }` (per OQ-1), so downstream builds against the published crate resolve while in-workspace builds use the local source.

`#[non_exhaustive]` on `NmeaError` and the public enums (per ADR-004 / ADR-009) is already done.

## When writing the project README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.

## Future work — out of scope for spec v2.0

- **AIS as a sendable `talker` payload.** The `nmea0183` crate already builds and parses `!AIVDM`/`!AIVDO` and armors the 6-bit payload, but spec v2.0 §5.1 lists exactly five message formats and AIS is not one of them. Exposing AIS in the `talker` message editor — whether as pre-armored raw bytes or as a structured per-message-type editor (Type 1/5/18/24…) — is a feature beyond the current spec. Revisit only with a spec amendment. See the ADR-012 context note and the 2026-05-22 discussion.

---

## Completed during the spec v2.0 upgrade

The sections below were open in TODO v1.0 and are now done; kept here so the history is not lost.

- **`nmea0183` source** — serde derives on all public types are gated behind the `serde` feature, and verified to compile both with and without it (`cargo build -p nmea0183` / `--features serde`).
- **`core::logging`** — `tracing-appender` added to workspace dependencies for rotating file output; dual-mode subscriber implemented (CLI writes to stdout/file; the GUI captures events into a `tracing_subscriber::Layer` that forwards to the UI thread via `crossbeam-channel`).
- **`core::profile`** — schema v2 with `CURRENT_VERSION` checked on every load; `#[serde(default)]` on all fields; `#[non_exhaustive]` on profile enums. OQ-2 (`toml = "1"` is sufficient) and OQ-3 (profiles use a `talker`-side NMEA representation, so the `nmea0183` `serde` feature stays off) are resolved — see the Open questions section of the ADR.
