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
- [x] **CLI** — `listener --profile <toml>` or quick `--udp/--tcp/--serial [--baud] [--nmea]`;
  loads/validates config, starts channels via the orchestrator, streams `RuntimeEvent`s,
  Ctrl-C → graceful shutdown. Thin presentation layer (§3); verified end-to-end.
- [ ] GUI (egui/eframe) — not started

Modules outside the §147 order (§128):

- [x] `decode/` — NMEA0183 decoder (§33–§39/§160), wired as the §107 pipeline stage
- [x] `config/` — `Profile`/`ChannelConfig` schema (§72–§80), serde/TOML load+save, schema-version refusal (§72.1), per-channel validation (§71), templates (§82–§85). **Wired**: the config→runtime mapping (`ExtractionConfig`→extractor, `*Config`→transport, `DecoderConfig`→decoder) lives in `runtime::build` and is driven by the orchestrator (see *Wiring* below).
- [x] `retention/` — `RetentionStore` §143; `MessageRetention` (count + byte limits, §88) and `CountBounded` (events/warnings/errors), backstop (§80). **Wired**: the pipeline's retention is `MessageRetention` (count + byte), fed per-channel from `RetentionConfig`; the plain `DropOldestQueue` now backs only the bounded display history (see *Wiring* below).
- [x] `diagnostics/` — owns the diagnostic model (`Diagnostic`/`DiagnosticSeverity`, moved here
  from `runtime::queue` and re-exported; §95 `ErrorCategory`), `DiagnosticLog` (per-type
  event/warning/error count retention, §88), and `init_logging` (tracing, §114). **Wired**:
  the pipeline retains diagnostics in a `DiagnosticLog`; the orchestrator feeds its per-type
  limits from `RetentionConfig` (`channel_caps`), completing the §80 retention limit set. The
  §99 drop-low-priority `DiagnosticsQueue` remains in `queue.rs` as a tested fan-out-edge utility.

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
  wires it for serial/UDP from `DecoderConfig`, and `start_tcp_listener` now takes a
  per-connection `make_decoder` factory so accepted TCP connections inherit the listener's
  decoder (§16.2). *Still open*: per-connection **recording** is deferred. §59 now defines a
  generated naming scheme, so the blocker is narrower — there is no naming/ownership rule for
  runtime TCP **connection** recordings (which identity goes in the filename: listener id,
  connection id, remote addr, accept time?) and connections are never persisted (§16.3).
- [x] **Recorder ↔ pipeline (raw + display)** — Raw Recording feeds the real
  `record::Recording` task at the chunk tap; overflow faults it, emits
  `RuntimeEvent::RecordingFaulted` + a diagnostic, reception continues (§56.1). Display
  Recording renders each Message for the primary view and feeds `DisplayFileRecorder`,
  running **regardless of pause** (§58); `finish` finalizes both. The orchestrator creates
  the recorder (`RecordingMode::Raw`→raw, `Display`→display of the primary view) at Start
  from `RecordingConfig`; an enable failure surfaces `WarningRaised` without faulting the
  channel (§55). *Still open*: per-view display-recording config (v1 records the primary
  view); TCP-connection recording is deferred.
- [x] **Retention ↔ pipeline** — pipeline retention is `retention::MessageRetention`
  (count **and** byte limits, §88); the orchestrator derives per-channel limits from
  `RetentionConfig`. *Still open*: event/warning/error retention limits (§80) need the
  `diagnostics` module; display history is still a plain count-bounded queue.
- [x] **Transport error reporting (§94/§101)** — all transports classify why they ended
  (`TransportOutcome::{Cancelled, Completed, Faulted(reason)}`). A spontaneous transport
  fault surfaces `ChannelFaulted`: data channels via a per-channel fault monitor
  (`spawn_monitored_channel`), TCP connections via the supervisor (+ `TcpClientDisconnected`).
  State reconciliation is now eager (ADR-006): the fault monitor flips a shared per-channel
  `Arc<AtomicBool>` that `Listener::state` and command validation read, so a spontaneously
  faulted data channel reports `Faulted` immediately (not lazily on the next command). The
  event remains authoritative for observers. The loss-observability boundary is classified in
  ADR-007: recorder overflow is reported with a truncation point; a sustained serial reader
  stall is reported through the transport→diagnostics seam (`TransportNotice` → pipeline
  records a Warning `Diagnostic` + emits `WarningRaised`, visible in a live snapshot);
  kernel-dropped UDP is fundamentally undetectable and is never fabricated. *Refinements
  still open*: a TCP listener-acceptor fault isn't reconciled through the flag (v1).
- [x] **Multi-view display + pause** — the pipeline holds N Display Views (§48) keyed by
  `DisplayViewId`, each with a shared `DisplayViewHandle` pause flag. Pausing one view
  freezes only its accumulation; reception/recording/numbering/retention and other views
  are unaffected (§11/§50). Wired through the orchestrator: it creates one runtime view per
  `DisplayConfig` view, stores the handles, and exposes `display_views`/`pause_display`/
  `resume_display(channel, view)` (the `RuntimeCommand::PauseDisplay` vocabulary).
- [x] **On-demand observability surface (§137, ADR-006)** — the *pull* half of
  observability. `runtime::snapshot` defines `ChannelSnapshot` (retained decoded Messages,
  per-view display history, diagnostics by severity, recording state); the pipeline serves
  snapshot requests between reads via a oneshot reply on a bounded request channel, so a
  live UI reads retained content (incl. decoder annotations) without owning or blocking the
  pipeline. `Listener::snapshot(id)` / `RunningChannel::snapshot()` expose it; the
  `RuntimeEvent` stream stays authoritative for liveness. *Still open*: per-connection TCP
  snapshots (the supervisor keeps no per-connection handle) — deferred like per-conn recording.

### v1 scoping decisions (accepted TCP connections)

Two capabilities are **intentionally deferred for v1** — not bugs, not "wire it up"
gaps. Accepted TCP connections already meet §16.2 inheritance parity (extraction +
decoder); these are separate, deliberately-scoped omissions:

- **Per-connection recording — deferred.** §59 (v1.2) now defines a generated naming
  scheme, so the blocker is narrower: there is no naming/ownership rule for runtime
  TCP **connection** recordings — which identity belongs in the filename (listener id,
  connection id, remote addr, accept time?) — and connections are never persisted
  (§16.3). Decide that rule before wiring it; do not invent it (AGENTS.md §1).
- **Per-connection snapshots — deferred unless the GUI needs them.** The TCP supervisor
  keeps no per-connection pipeline handle, so a connection isn't a snapshot target today.
  Revisit only if the GUI's connection-level live inspection requires it; if so, it's a
  scoped addition (retain per-connection snapshot senders in the supervisor), not a
  redesign.

---

## Acceptance (§153–§160 — the product-ready bar)

Not all acceptance criteria are proven end-to-end yet (some are checked or partial
below; the v1.2 addendum §161–§168 is mostly implemented now — only §165 remains,
with §167 UDP-only). Each generally needs the wiring above first.

- [ ] §153 Channel operation — create from templates, start/stop independently, run many
- [ ] §154 TCP connections — accept, one channel/client, independent, not persisted
- [ ] §155 Message processing — Stream/Delimiter/Fixed-Length/sync-marker selectable; UDP datagram → Message
- [~] §156 Display — multi-view + pause-without-affecting-reception/numbering/retention proven
  end-to-end (`pausing_a_display_view_freezes_only_that_view`); pause-without-affecting-*recording*
  is unit-proven (`display_recording_continues_while_the_view_is_paused`). *Still to add*:
  Raw/Rendered/Hex rendering selection asserted through a running channel.
- [x] §157 Metadata/timing — number, byte count, arrival timestamp, reception duration, and NMEA
  integrity asserted via a live snapshot (`snapshot_exposes_message_metadata_and_nmea_integrity`).
- [ ] §158 Recording — enable/disable per channel, Raw/Display, optional timestamps, no backfill
- [ ] §159 Profiles — save/load workspace; load does not start channels, begin recording, or restore runtime state
- [x] §160 NMEA0183 — decoder behavior proven through a running channel: a valid sentence reports
  integrity Valid + message type, a bad-checksum sentence is retained and annotated Invalid (§37).

### v1.2 acceptance addendum (spec §161–§168) — only §165 remains (§167 UDP-only)

Per the v1.2 review: build acceptance/tests **before** implementing each, so the
runtime-surface expansion stays anchored to a product-readiness bar.

- [x] §161 Serial control lines — **DONE.** `transport::SerialControlLines` {rts,dtr,cts,dsr,dcd,ri};
  the serial receive loop services live RTS/DTR commands and polls input lines between bounded reads
  (never interferes with reception) via the extended `BlockingReader` seam + `SerialControlHooks`
  (command inbox + shared state cell + events). Orchestrator: a serial channel gets a control handle;
  `Listener::set_rts`/`set_dtr` (commands) + `serial_control_lines(id)` (pull) + `ControlLinesChanged`
  event (push, added to `RuntimeEvent`). Tested: loop applies a command + reports an input change (unit
  via a fake `ControlReader`); non-serial channel reports control unavailable (orchestrator unit). The
  full orchestrator serial path needs real hardware (loopback can't); only the loop logic is unit-tested.
- [x] §162 Auto-reconnect — **DONE.** `config::ReconnectPolicy` {enabled(default false), initial/max
  backoff ms, multiplier, max_attempts}; `ChannelConfig.reconnect`. The orchestrator has no background
  loop (ADR-006), so the app drives `Listener::reconnect_tick()`: it arms a backoff timer on first
  observing an effective-Faulted reconnect-enabled channel, then once due does a Stop+Start (reusing the
  tested lifecycle), backing off exponentially on failure and giving up after `max_attempts`. Events
  `ChannelReconnecting(id, attempt)` / `ChannelReconnected` / `ChannelReconnectGaveUp` (added to
  `RuntimeEvent` + CLI). CLI ticks reconnect every 500ms. TCP **connection** channels excluded (not in the
  registry; the listener doesn't dial out). Tested: success path + give-up-after-max (orchestrator unit).
- [x] §163 File rotation — **DONE.** `record::file_rotation` (`RotatingRawRecorder`/`RotatingDisplayRecorder`)
  writes `<channel>_<UTC-period><ext>` files per Hourly/Daily period, data-driven from each item's
  wall-clock, clean file boundaries (no gap/backfill); `RecordingConfig.file_rotation`; orchestrator builds a
  rotating recorder when rotation != None (destination = directory); filesystem-safe channel-name
  validation (§71). Tested: period keys/UTC, name safety, raw hour-boundary + display day-boundary
  rotation (unit), config rejection (unit), orchestrator named-file (integration). `.ssdat` (subsampled)
  still pending the subsampling feature.
- [x] §164 Subsampling — **DONE** (both acceptance sinks: display view + message recording).
  `config::Subsample` (None / EveryNth{n} / RateLimit{millis}); `runtime::subsample::Subsampler` (count-
  and time-based, advances over the full stream so it's pause-independent). **Display views**:
  `DisplayViewConfig.subsample` threaded per-view (`set_view_subsamples`), gating each view's history in
  dispatch → gapped (not renumbered) numbers; reception/numbering/retention unaffected. **Data file**:
  `RecordingConfig.subsample` on a Raw recording makes it a message-framed **`.ssdat`** (vs byte-exact
  `.dat`) — `pipeline::MessageRecorder` taps the post-extraction Message fan-out, feeding the raw recording
  task synthetic per-Message chunks, decimated; orchestrator routes Raw+subsample → `.ssdat` via a
  `DataRecorder` enum. Validation rejects `EveryNth{n:0}`. Tested: Subsampler none/count/rate (unit),
  config rejection (unit), orchestrator gapped-view + .ssdat decimated-bytes (integration). *Optional
  extras deferred* (beyond the §164 bar): display-recording (`.disp`) subsampling; per-connection TCP
  display-subsample inheritance.
- [ ] §165 Match rules & triggers — each condition fires its actions; record-from-match-forward; mark never touches raw
- [x] §166 Liveness — **DONE.** `runtime::activity::ActivityMeter` (bounded 5×1s ring) tracks
  `last_data_at` + rolling bytes/sec & msgs/sec; the pipeline records per chunk (ingest) and per
  Message (dispatch); surfaced in `ChannelSnapshot.activity` (`ChannelActivity`). Fact source only —
  consumers derive "idle" against their own threshold (unblocks MR&T `Idle`, §50.2). Tested: meter
  window/decay/zero (unit), orchestrator snapshot shows throughput + last-data (integration).
- [~] §167 Network live adjustment — **UDP core DONE.** `UdpConfig.recv_buffer_bytes` (SO_RCVBUF, set
  before bind via `socket2` — the kernel-UDP-drop lever, §101) + `multicast_interface` (NIC selection for
  the group join); `build_udp` wires both; changes take effect via the §13 apply-pending restart. Added
  `socket2` dep. Tested: `recv_buffer_size_is_applied_at_bind` (unit) + existing UDP bind/recv still pass.
  *Deferred*: `TcpListenerConfig.recv_buffer_bytes` (field added, not yet applied to the listener socket);
  truly-live no-restart commands (`SetReceiveBuffer`/`JoinMulticast`/`LeaveMulticast` into the running UDP
  task) — spec §76.1 marks live RCVBUF best-effort, so apply-pending restart covers it for v1.
- [x] §168 Disk-space guard — **DONE.** `config::DiskGuard` {min_free: `DiskThreshold` (Bytes|Percent),
  on_low: `LowDiskAction` (Warn|StopRecording)}; `RecordingConfig.disk_guard`. The guard lives in the
  pipeline (which owns the recorders): `run_channel` polls free space every 5s (`fs2`, off the hot path)
  via `check_disk_guard`. On a low condition it records a warning diagnostic and emits `DiskSpaceLow`
  once per episode (debounced; re-arms when space recovers); if the policy is `StopRecording` it
  finalizes **all** recordings (raw/.ssdat/display) cleanly and emits `RecordingStoppedLowDisk` while
  reception/extraction/display/retention continue (§96). Events added to `RuntimeEvent` + CLI. Orchestrator
  wires the guard only when both a guard and a recording destination are configured. Added `fs2` dep.
  Tested: `disk_is_low` byte/percent/zero-total (unit), pipeline stop-once + both events + idempotence
  with a real recorder and `min_free = u64::MAX` (unit/async). *Deferred*: the periodic poll is fixed at
  5s (not yet configurable).

---

## Tests required (spec §150–§152)

- [x] Message extraction matrix (§150 boundary cases) — `extract/`
- [x] NMEA checksum / Standard vs Strict / proprietary / AIS (§151) — `decode/`
- [x] Backpressure substrate (§152): display drop-oldest, recorder fault-not-stall, retention eviction preserves numbering, diagnostics drop-low-priority — `runtime/`
- [x] Graceful vs forced shutdown of a running channel (drain vs abandon; both terminate) — `runtime::channel`
- [x] Reader stall reportable as transport-specific loss (§99, §101) — the serial reader
  watches `blocking_send`; a stall beyond `STALL_WARNING` (250 ms) sends a
  `TransportNotice::ReceptionStalled { stalled }` once per episode (unquantified — UART
  overrun isn't countable from userland), never by dropping data. The pipeline records it
  as a Warning `Diagnostic` (named, with the stall duration) **and** emits `WarningRaised`,
  so it shows up in a live snapshot's diagnostics. Boundary + seam in ADR-007.
- [~] Acceptance-level integration tests (`listener/tests/`, black-box via public API) — first
  suites landed: loopback UDP (datagrams numbered, clean stop, §153–§155), loopback TCP
  (accept → distinct connection id → delimited Message → stop terminates connections,
  §16), and profile behavior (round-trip, load-does-not-start §70/§159, TCP-connection
  rejected §16.3, unbounded-retention rejected §80), a TCP connection inheriting the
  listener's NMEA decoder, a live snapshot exposing a running channel's decoded Messages
  + per-view display history, display-view **pause** freezing only that view via snapshots
  (§156), and Message **metadata/timing + NMEA integrity** via snapshots (§157/§160).
  *Still to add*: Raw/Rendered/Hex rendering selection through a running channel; recording
  enable/disable acceptance (§158).

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
- Size-based file rotation / old-file pruning / advanced filename templating
  (time-based rotation is supported — spec §59)
- Pre-trigger / pre-match recording capture (spec §50.2)
- Persistent diagnostic log rotation
- Hard real-time guarantees
