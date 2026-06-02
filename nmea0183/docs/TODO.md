# TODO — nmea0183

Implementation reminders specific to the `nmea0183` library crate. Workspace- and
`talker`-level tasks live in [`talker/docs/TODO.md`](../../talker/docs/TODO.md).
Not architectural decisions (those go in the ADR's Open questions section).

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

---

## Completed during the spec v2.0 upgrade

Kept here so the history is not lost.

- **`nmea0183` source** — serde derives on all public types are gated behind the `serde` feature, and verified to compile both with and without it (`cargo build -p nmea0183` / `--features serde`).
