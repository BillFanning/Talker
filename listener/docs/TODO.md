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

## v1 → v2 strip (ADR-010 build order; spec §147 for fresh order)

Sequence matters: tests first, then remove in dependency order, keeping the build
green at each step.

- [ ] Write the v2 invariant tests first (spec §150):
  - [ ] byte-pattern find across receive-chunk boundaries (§50.2)
  - [ ] idle-rule firing and re-arming
  - [ ] UDP datagram boundary preserved as a reception/recording detail only
  - [ ] raw recording byte-exactness (`.raw`)
  - [ ] display recording reflects the rendered view (`.disp`)
  - [ ] pause affects neither reception nor recording
  - [ ] fan-out overflow: single backpressure edge (Transport→Pipeline), consumers drop/fault
  - [ ] schema v1 profile refused
- [ ] Remove `extract/` (and the `MessageExtractor` seam in the pipeline)
- [ ] Remove `decode/`; drop the `nmea0183` dependency from `listener/Cargo.toml`
- [ ] Collapse the pipeline: transport → bounded queue → non-blocking fan-out
      (raw recorder, scrollback, display recorder, find/triggers, diagnostics) — §102
- [ ] Replace message retention with **byte-bounded** stream scrollback (§80, §88)
- [ ] Remove subsampling and `.ssdat`; rename raw extension `.dat` → `.raw` (§53, §59)
- [ ] Config schema: drop `extraction`/`decoder`/`subsample`/view `source`/`annotations`;
      byte-based `RetentionConfig`; **bump `schema_version`**, refuse v1 (§72)
- [ ] Re-root Find & Triggers on the stream: `BytePattern` (cross-chunk carry) + `Idle`;
      `Highlight`/`Mark` anchored on byte offset (§50.2)
- [ ] GUI: remove framing selector, NMEA-decode toggle, Stream↔Messages source switch,
      per-message #/timestamp toggles; the Stream viewer is the only viewer
- [ ] Events: drop `MessageReceived`; liveness stays byte-based (§137, §166)
- [ ] Test cleanup: delete extraction/decoder/NMEA/subsample tests; keep transports,
      recording, rotation, backpressure, control lines, reconnect, liveness

## v2 feature gaps (after the strip)

- [ ] Find & Triggers runtime: cross-chunk `BytePattern` scanner with carry,
      byte-offset match records in the snapshot (§50.2)
- [ ] Highlight rendering in the stream view (byte-range styling)
- [ ] `Mark` markers in display + `.disp` (never `.raw`)
- [ ] Live `Record` begin/stop via `EnableRecording`/`DisableRecording` (no restart)
- [ ] Recording timestamp sidecar for `.raw` (byte-offset keyed, §57)
- [ ] Match-rule editor UI (rules currently arrive only via profiles)
- [ ] Profiles: save/load wired end-to-end against the v2 schema (§67–§71)
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
