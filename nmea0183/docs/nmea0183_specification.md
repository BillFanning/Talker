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
(`!AIVDM`/`!AIVDO` with 6-bit payload armoring). It has no dependency on `talker`.

## To document here

- [ ] Public API surface: sentence types, `TalkerId`, `ProprietarySentence`, `AisSentence`.
- [ ] Checksum semantics (inline XOR; `$PRDID` carries none by convention).
- [ ] Extensibility model (`Custom(String)` variants) — cross-reference ADR-009.
- [ ] `serde` feature gating.
- [ ] Error model (`NmeaError`) — cross-reference ADR-004.
