# TODO — Listener

Implementation reminders for the `listener` crate. Not architectural decisions
(those go in [`ADR.md`](ADR.md) or the spec). Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Cross off items as they are completed. Add new ones inline as they come up.

**Read this first — two different bars.** A checked box under *Module substrate*
means the module exists and is unit-tested in isolation. It does **not** mean the
feature is wired into a running Channel or that any §153–§160 acceptance criterion
is proven end-to-end. Those are tracked separately under *Wiring / integration*
and *Acceptance*. Do not infer product readiness from the substrate list alone.

---

## Module substrate (built + unit-tested in isolation)

Implementation order per spec §147 (do **not** start with the GUI). Each unit is a
`src/` module (single crate, per ADR-004 / spec §127):

- [x] `core/` — common types, Message/metadata model, `ChannelId`, state enums (incl. §9 transition table), errors
- [x] `extract/` — `MessageExtractor`; stream / delimiter / fixed-length (full §150 matrix)
- [x] `runtime/` skeleton — queue policies, numbering, pipeline, channel/TCP orchestration helpers
- [x] queue / backpressure tests (the Transport→Extractor path, §99/§152)
- [x] UDP transport
- [x] serial transport (dedicated OS thread + bounded read timeout for cancellation, §111)
- [x] TCP listener/connection transport (runtime mints `ChannelId` per accepted connection)
- [x] recording — `RawRecorder`/`DisplayRecorder`, file impls, `OverwritePolicy`, fault/finalize (§56.1)
- [x] display — encodings, Raw/Rendered/Hex, character rendering, wrapping, `Renderer`
- [ ] GUI / CLI

Modules outside the §147 order (§128):

- [x] `decode/` — NMEA0183 decoder (§33–§39/§160), wired as the §107 pipeline stage
- [x] `config/` — `Profile`/`ChannelConfig` schema (§72–§80), serde/TOML load+save, schema-version refusal (§72.1), per-channel validation (§71), templates (§82–§85). *Not yet wired*: the config→runtime mapping (`ExtractionConfig`→extractor, `*Config`→transport, `DecoderConfig`→decoder) lives in the orchestrator step.
- [x] `retention/` — `RetentionStore` §143; `MessageRetention` (count + byte limits, §88) and `CountBounded` (events/warnings/errors), backstop (§80). *Not yet wired*: pipeline still uses a single message-count `DropOldestQueue`; swap in `MessageRetention` during the config/orchestrator wiring.
- [ ] `diagnostics/` — events/warnings/errors retention + tracing logging (still a stub; today only an in-pipeline `DiagnosticsQueue`)

## Scaffolding

- [x] Stand up `lib.rs` declaring the §127 modules + thin `main.rs` shim (talker ADR-014 shape).

_OQ-L1 (single crate vs. multi-crate) is resolved — see ADR-004._

---

## Wiring / integration (substrate exists but not yet connected end-to-end)

These are the "looks done but isn't connected" gaps. None are proven by acceptance
tests yet.

- [x] **Runtime command/orchestrator layer** — `runtime::Listener` owns the channel
  registry, drives the §9 state machine, builds live transports/extractors/decoders from
  config (`runtime::build`), opens/binds at Start (→Faulted on failure, §71), and does
  `start`/`stop`/`set_pending_config`/`apply_pending` (§13) + `shutdown` (§113). Commands
  are async methods, not a background command-loop task; that's fine for v1.
- [x] **Decoder entrypoint** — `spawn_channel_tasks` takes a decoder; the orchestrator
  wires it for serial/UDP from `DecoderConfig`. *Still open*: TCP **connection** channels
  are undecoded (`start_tcp_listener` needs a per-connection decoder factory).
- [ ] **Recorder ↔ pipeline** — pipeline still uses the synchronous `FaultOnFullQueue`
  stub for the raw tap and emits a diagnostic, not `RuntimeEvent::RecordingFaulted`; the
  real `record::` file tasks are not connected. The orchestrator currently starts every
  channel with recording **off** regardless of `RecordingConfig`. Display Recording not
  connected to the display fan-out.
- [ ] **Retention ↔ pipeline** — pipeline still uses a single-count `DropOldestQueue`;
  swap in `retention::MessageRetention` (count+byte limits) driven by `RetentionConfig`.
- [ ] **Transport error / loss reporting (§94/§101)** — UDP/TCP/serial `break` on fatal
  read/accept errors with `TODO` markers; no warning/error events, no transport-specific
  loss reporting. Report only *observable* loss (kernel-dropped UDP is not detectable).
- [ ] **Multi-view display + pause** — pipeline has one display queue, not N views keyed
  by `DisplayViewId`; `DisplayState`/Pause-Resume commands (§11/§50) are unimplemented.

---

## Acceptance (§153–§160 — the product-ready bar)

All unverified end-to-end. Each generally needs the wiring above first.

- [ ] §153 Channel operation — create from templates, start/stop independently, run many
- [ ] §154 TCP connections — accept, one channel/client, independent, not persisted
- [ ] §155 Message processing — Stream/Delimiter/Fixed-Length/sync-marker selectable; UDP datagram → Message
- [ ] §156 Display — Raw/Rendered/Hex, multi-view, config, pause without affecting reception/recording
- [ ] §157 Metadata/timing — number, byte count, arrival, optional duration, optional integrity
- [ ] §158 Recording — enable/disable per channel, Raw/Display, optional timestamps, no backfill
- [ ] §159 Profiles — save/load workspace; load does not start channels, begin recording, or restore runtime state
- [ ] §160 NMEA0183 — decoder behavior (substrate done; prove through a running channel)

---

## Tests required (spec §150–§152)

- [x] Message extraction matrix (§150 boundary cases) — `extract/`
- [x] NMEA checksum / Standard vs Strict / proprietary / AIS (§151) — `decode/`
- [x] Backpressure substrate (§152): display drop-oldest, recorder fault-not-stall, retention eviction preserves numbering, diagnostics drop-low-priority — `runtime/`
- [x] Graceful vs forced shutdown of a running channel (drain vs abandon; both terminate) — `runtime::channel`
- [ ] Reader stall reportable as transport-specific loss (§99, §101) — blocked on transport error/loss wiring
- [ ] Acceptance-level (§150 product areas): channel operation, TCP connections, display/pause, metadata/timing, recording end-to-end, profiles — blocked on the wiring above

## Future work — deferred from Version 1 (spec Appendix A)

Out of scope for v1; revisit only with a spec amendment:

- Automatic protocol detection
- Protocol field extraction
- CSV / JSON / structured-semantic export
- Session replay
- Plugin architecture
- TCP client mode
- Distributed operation
- Advanced synchronization recovery
- File rotation
- Persistent diagnostic log rotation
- Hard real-time guarantees
