# TODO — Talker

Implementation reminders for the `talker` application and the workspace as a whole.
Tasks specific to the `nmea0183` library live in
[`nmea0183/docs/TODO.md`](../../nmea0183/docs/TODO.md). Not architectural decisions
(those go in the ADR's Open questions section).

Cross off items as they are completed. Add new ones inline as they come up.

---

## When writing the project README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.

## Future work — out of scope for spec v2.0

- **AIS as a sendable `talker` payload.** The `nmea0183` crate already builds and parses `!AIVDM`/`!AIVDO` and armors the 6-bit payload, but spec v2.0 §5.1 lists exactly five message formats and AIS is not one of them. Exposing AIS in the `talker` message editor — whether as pre-armored raw bytes or as a structured per-message-type editor (Type 1/5/18/24…) — is a feature beyond the current spec. Revisit only with a spec amendment. See the ADR-012 context note and the 2026-05-22 discussion.

---

## Completed during the spec v2.0 upgrade

The sections below were open in TODO v1.0 and are now done; kept here so the history is not lost.

- **`core::logging`** — `tracing-appender` added to workspace dependencies for rotating file output; dual-mode subscriber implemented (CLI writes to stdout/file; the GUI captures events into a `tracing_subscriber::Layer` that forwards to the UI thread via `crossbeam-channel`).
- **`core::profile`** — schema v2 with `CURRENT_VERSION` checked on every load; `#[serde(default)]` on all fields; `#[non_exhaustive]` on profile enums. OQ-2 (`toml = "1"` is sufficient) and OQ-3 (profiles use a `talker`-side NMEA representation, so the `nmea0183` `serde` feature stays off) are resolved — see the Open questions section of the ADR.
