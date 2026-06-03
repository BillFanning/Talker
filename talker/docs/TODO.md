# TODO — Talker

Implementation reminders for the `talker` application and the workspace as a whole.
Tasks specific to the `nmea0183` library live in
[`nmea0183/docs/TODO.md`](../../nmea0183/docs/TODO.md). Not architectural decisions
(those go in the ADR's Open questions section).

Cross off items as they are completed. Add new ones inline as they come up.

---

## Profiles

- [ ] **Make the profile directory user-configurable.** Default stays the OS config
  dir (`dirs::config_dir()/talker/profiles`, `core::profile::default_dir`) — the safe,
  always-writable choice. Add an override so users can point talker at a directory of
  their choosing (e.g. a `--profile-dir` CLI flag, a `TALKER_PROFILE_DIR` env var, and/or
  a GUI setting), which also enables a portable "profiles next to the .exe" layout
  without making it the default. Sample profiles ship in `talker/profiles/`.

## When writing the project README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.

## Future work — out of scope for spec v2.0

- **AIS as a sendable `talker` payload.** The `nmea0183` crate already builds and parses `!AIVDM`/`!AIVDO` and armors the 6-bit payload, but spec v2.0 §5.1 lists exactly five message formats and AIS is not one of them. Exposing AIS in the `talker` message editor — whether as pre-armored raw bytes or as a structured per-message-type editor (Type 1/5/18/24…) — is a feature beyond the current spec. Revisit only with a spec amendment. See the ADR-012 context note and the 2026-05-22 discussion.

- **Manual transmit / inject for troubleshooting.** Captured here because transmit is `talker`'s job, not `listener`'s (listener is strictly receive-side). Beyond talker's scheduled/profile-driven sends, a troubleshooting workflow wants **ad-hoc, one-shot injection** — type or pick a payload and fire it once at a serial port or network endpoint to provoke a device, while `listener` observes the response on the same or another channel. This is the natural talker counterpart to the listener troubleshooting use case (see the listener "primary use cases" notes, 2026-06). A real feature here needs a spec amendment: define how one-shot/manual sends relate to the §5.1 message formats and the scheduler, and whether it's CLI, GUI, or both. Revisit only with that amendment.

---

## Completed during the spec v2.0 upgrade

The sections below were open in TODO v1.0 and are now done; kept here so the history is not lost.

- **`core::logging`** — `tracing-appender` added to workspace dependencies for rotating file output; dual-mode subscriber implemented (CLI writes to stdout/file; the GUI captures events into a `tracing_subscriber::Layer` that forwards to the UI thread via `crossbeam-channel`).
- **`core::profile`** — schema v2 with `CURRENT_VERSION` checked on every load; `#[serde(default)]` on all fields; `#[non_exhaustive]` on profile enums. OQ-2 (`toml = "1"` is sufficient) and OQ-3 (profiles use a `talker`-side NMEA representation, so the `nmea0183` `serde` feature stays off) are resolved — see the Open questions section of the ADR.
