# nmea0183 — Library Specification

**Crate:** nmea0183
**Status:** Placeholder — to be written

---

> This is a scaffold. The `nmea0183` crate is currently specified implicitly by its
> rustdoc, its tests, and the decisions in [`ADR.md`](ADR.md). Fill this document in
> before crates.io publication (see [`TODO.md`](TODO.md)).

## Scope

`nmea0183` handles NMEA 0183 sentence construction, parsing, checksum, talker IDs,
proprietary sentences (`$PRDID`, `$PASHR`, arbitrary `$P`), and AIS sentences
(`!AIVDM`/`!AIVDO` with 6-bit payload armoring). `SentenceType::time_fields()`
exposes the explicit zero-based time/date field positions applications need for
live UTC substitution (ADR-028), and `format_utc_time` supplies their shared
fixed-width UTC field rendering without taking a clock-library dependency (ADR-030).
The crate has no dependency on either application.

## To document here

- [ ] Public API surface: sentence types and `TimeFieldKind`, `format_utc_time`,
      `TalkerId`, `ProprietarySentence`, `AisSentence`.
- [ ] Checksum semantics (inline XOR; `$PRDID` carries none by convention).
- [ ] Extensibility model (`Custom(String)` variants) — cross-reference ADR-009.
- [ ] `serde` feature gating.
- [ ] Error model (`NmeaError`) — cross-reference ADR-004.
