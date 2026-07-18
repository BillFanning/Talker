# Architecture Decision Record — nmea0183

**Crate:** nmea0183
**Status:** Accepted

---

This file records decisions specific to the `nmea0183` library crate. Workspace-level
and `talker`-application decisions live in [`talker/docs/ADR.md`](../../talker/docs/ADR.md).
Several of those still touch `nmea0183` (its extraction as a separate crate in ADR-001,
the `thiserror` error model in ADR-004, the inline XOR checksum in ADR-007, the workspace
MSRV in ADR-008) — see that file for the reasoning. ADR numbers are shared across both
files and never reused, so cross-references stay valid.

---

## ADR-009 — Talker ID and sentence type extensibility in `nmea0183`

**Context:** NMEA 0183 has ~36 standard talker IDs and many sentence types. New proprietary sentences (`$P...`) are encountered regularly in marine and survey equipment.

**Decision:**
- Standard talker IDs are represented as an enum with a `Custom(String)` variant for arbitrary two-character IDs.
- Sentence types follow the same pattern: an enum with a `Custom(String)` variant.
- Proprietary sentences use a dedicated `ProprietarySentence` type with named variants for known formats (`Prdid`, `Pashr`) and a `Raw` variant for arbitrary `$P` sentences.

**Consequences:**
- Named proprietary sentences (`$PRDID`, `$PASHR`) get field-level construction and validation.
- The `Raw` variant accepts any manufacturer code and comma-separated field string with optional checksum — no validation beyond checksum computation.
- The `$PASHR` GNSS quality field (field 10) is exposed as a raw `u8` rather than an enum, because Trimble and Novatel define the values differently. The crate documentation must record both vendor conventions explicitly.
- `$PRDID` does not include a checksum by protocol convention; the builder must not append one.

---

## ADR-028 — Sentence types own explicit live UTC field metadata

**Status:** Accepted 2026-07-17.

**Context:** Applications that refresh NMEA time/date fields need protocol knowledge:
the same logical instant occupies different zero-based field positions in GGA, GLL,
RMC, ZDA, and related sentences. Keeping that table in a GUI or sender would duplicate
NMEA semantics and let future consumers disagree.

**Decision:** `SentenceType::time_fields()` returns a static slice of
`(field_index, TimeFieldKind)` entries. `TimeFieldKind` distinguishes UTC time,
`ddmmyy`, day, month, and four-digit year. The mapping is explicit, never inferred
from mnemonic names: ASHR/BWC/BWR/GBS/GGA/GNS/GRS/GST/ZFO/ZTG use UTC field 0; GLL
uses UTC field 4; RMC uses UTC field 0 and date field 8; ZDA uses fields 0–3 for UTC,
day, month, and year. Every other standard or custom type returns an empty slice.
Positions are relative to `NmeaSentence::fields`, excluding talker and mnemonic.

`TimeFieldKind` is deliberately exhaustive: a new formatting kind must make consumers
handle it at compile time. The existing extensible `SentenceType::Custom` boundary is
unchanged.

**Consequences:** NMEA field knowledge lives in the publishable library and Talker
only supplies an instant and formatting policy. An empty mapping means substitution
is unsupported, not that a sentence can never contain time-like data. Unit tests pin
every supported entry, sorted unique positions, and empty custom/unsupported types.

---

## Open questions

**OQ-4 — `nmea0183` library MSRV policy.** ADR-008 sets the workspace MSRV to current stable Rust. The `nmea0183` library, intended for crates.io publication, may benefit from a looser MSRV to accommodate cautious downstream users. The policy (e.g., N-6 months of stable releases) and the mechanism (per-crate `rust-version` override) are deferred to a future ADR when publication approaches. See ADR-008 (in `talker/docs/ADR.md`) for context.
