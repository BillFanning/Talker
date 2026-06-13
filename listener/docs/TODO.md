# TODO — Listener

Implementation reminders for the `listener` crate. Not architectural decisions
(those go in [`ADR.md`](ADR.md) or the spec). Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

**Reset for spec v2.0 (stream-only, ADR-010).** The v1 message-era checklist is
archived in git history (`git show 5a8f491:listener/docs/TODO.md`). Everything
below tracks the v1→v2 strip and the v2 feature set. Do not resurrect items from
the old list — Messages, extraction, decoding, and subsampling are gone.

Cross off items as they are completed. Add new ones inline as they come up.

---

## v1 → v2 strip (ADR-010) — DONE

The strip is complete and the workspace builds clean (`cargo test -p listener`,
`clippy -D warnings`, `fmt --check` all pass). Landed across commits `5aea4dc`
(decoder + `nmea0183` removal), `44008ed` (extract removal, pipeline collapse,
Message-model removal). Everything below this block is verified done:

- [x] Write the v2 invariant tests first (spec §150):
  - [x] byte-pattern find across receive-chunk boundaries (§50.2) — see note in
        the feature gaps below: the **scan still operates per-chunk**, so a pattern
        split across two reads is not yet matched; the test asserts within-chunk.
  - [x] idle-rule firing and re-arming
  - [x] UDP datagram boundary preserved as a reception/recording detail only
  - [x] raw recording byte-exactness (`.raw`)
  - [x] display recording reflects the rendered view (`.disp`)
  - [x] pause affects neither reception nor recording
  - [x] fan-out overflow: single backpressure edge (Transport→Pipeline), consumers drop/fault
  - [x] schema v1 profile refused (`CURRENT_VERSION = 2`)
- [x] Remove `extract/` (and the `MessageExtractor` seam in the pipeline)
- [x] Remove `decode/`; drop the `nmea0183` dependency from `listener/Cargo.toml`
- [x] Collapse the pipeline: transport → bounded queue → non-blocking fan-out
      (raw recorder, scrollback, display recorder, find/triggers, diagnostics) — §102
- [x] Replace message retention with **byte-bounded** stream scrollback (§80, §88);
      `retention/` keeps only `CountBounded` for events/warnings/errors
- [x] Remove subsampling and `.ssdat`; rename raw extension `.dat` → `.raw` (§53, §59)
- [x] Config schema: drop `extraction`/`decoder`/`subsample`/view `source`/`annotations`;
      byte-based `RetentionConfig`; **bump `schema_version`**, refuse v1 (§72)
- [x] Re-root Find & Triggers on the stream: `BytePattern` + `Idle`;
      matches anchored on `byte_offset` (§50.2). Cross-chunk carry now wired (a
      pattern split across two reads matches; see below).
- [x] GUI: remove framing selector, NMEA-decode toggle, Stream↔Messages source switch,
      per-message #/timestamp toggles; the Stream viewer is the only viewer
- [x] Events: drop `MessageReceived`; liveness stays byte-based (§137, §166)
- [x] Test cleanup: deleted extraction/decoder/NMEA/subsample tests; kept transports,
      recording, rotation, backpressure, control lines, reconnect, liveness. Also
      removed the `Message`/`MessageBytes`/`MessageMetadata` model and
      `MessageRetention`; renamed `core/message.rs` → `core/timing.rs` (keeps the
      still-needed `ChunkTime` / `ChunkTimestamp`).

## v2 feature gaps (after the strip)

- [x] Find & Triggers runtime: cross-chunk `BytePattern` scanner **with carry** —
      a pattern split across two reads now matches (`MatchRuleSet` keeps the prior
      chunk's tail and scans `carry ++ chunk`, reporting only matches ending in the
      new chunk). Each firing carries its true `match_offset`. **Measurement:** a
      boundary-split firing records a where/why event diagnostic and increments
      `match_boundary_saves`, surfaced in both `ChannelStats` and `ChannelSnapshot`
      (the how-often). `reset_stream` drops the carry on Stop/Start.
- [ ] Highlight rendering in the stream view (byte-range styling)
- [ ] `Mark` markers in display + `.disp` (never `.raw`)
- [ ] Match-`Record` to a Display/`Both` target: arm the display recorder. The
      `RecordTarget::Display`/`Both` variants exist and the pipeline accepts them,
      but only the Raw side is driven today (`apply_pending_records` skips
      display-only); the display portion needs per-view display-recorder arming.
- [x] `DisplayViewConfig.hex_grouping` schema field (spec §1431) — `HexGrouping
      { bytes_per_group, groups_per_line }` added with `#[serde(default)]`, so
      profiles round-trip it (`b8960a1`). Pinned by `hex_grouping_round_trips_through_
      a_profile` + `a_profile_without_hex_grouping_loads_with_the_default`.
- [ ] Consume `hex_grouping` in the Hex renderer: `display::DisplayView` still uses
      its own `hex_bytes_per_line`/`hex_separator`; map the config field onto them so
      the persisted grouping actually drives the Hex view (with a GUI control).
- [ ] Live `Record` begin/stop without a restart. Recording is wired only from
      `RecordingConfig` at channel start. Per ADR-012 the path is a new `Listener`
      method (e.g. `enable_recording`/`disable_recording`) plus the internal command
      channel into `run_channel` (ADR-008) — not a top-level command enum — with a
      GUI emitter (`UiCommand`) on top.
- [ ] Recording timestamp sidecar for `.raw` (byte-offset keyed, §57)
- [ ] Match-rule editor UI (rules currently arrive only via profiles)
- [x] `RuntimeCommand`'s role (§136) — resolved by ADR-012: removed the vestigial
      `core::RuntimeCommand` enum; the command surface is the `Listener` async method
      API (the GUI's `UiCommand` is the bridge transport). Spec §136 rewritten to
      match (v2.0.1).
- [x] Profiles: save/load wired end-to-end against the v2 schema (§67–§71)
      (`6b17021`). The GUI Profile menu (Save / Save As… / Load) routes through
      `UiCommand::{SaveProfile,LoadProfile}`; the driver gathers configs from the
      authoritative `Listener` for save, and on load parses+validates first then
      swaps the channel set (§70 — channels restore Stopped). Pinned by
      `save_then_load_round_trips_the_workspace_through_the_driver` +
      `loading_a_missing_profile_errors_without_touching_the_workspace`.
- [ ] Export (stream scrollback → file, §60–§63) — after GUI settles

## Carried over (still valid under v2)

- [ ] Disk-guard GUI exposure (§56.2)
- [ ] Per-connection recording for TCP connection channels (§16.2, deferred §59 naming)
- [ ] RS-422/485 phases (§14.4)
- [ ] CLI parity with GUI for the v2 surface

## Future work — deferred (spec Appendix A)

- Protocol decoders / field extraction (any return would be a new ADR)
- CSV / JSON / structured-semantic export; session replay
- Plugin architecture; TCP client mode; distributed operation
- Size-based file rotation / old-file pruning / filename templating
- Pre-trigger / pre-match recording capture (§50.2)
- Persistent diagnostic log rotation
- Hard real-time guarantees
