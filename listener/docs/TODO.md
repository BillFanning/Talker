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
  - [x] byte-pattern find across receive-chunk boundaries (§50.2) — cross-chunk carry
        is now wired (see the feature-gaps entry below); a pattern split across two
        reads matches.
  - [x] idle-rule firing and re-arming
  - [x] UDP datagram boundary preserved as a reception/recording detail only
  - [x] raw recording byte-exactness (`.raw`)
  - [x] display recording reflects the rendered view (`.disp`)
  - [x] pause affects neither reception nor recording
  - [x] fan-out overflow: single backpressure edge (Transport→Pipeline), consumers drop/fault
  - [x] older-schema profile refused (`CURRENT_VERSION = 3` — bumped to 2 by the v2
        strip, then to 3 by the Raw/Display split, ADR-013)
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
      new chunk). Rules fire **per occurrence** (a chunk holding three `$GPGGA`s
      fires a GGA rule three times, each at its own `match_offset`) — pinned by
      `every_occurrence_in_a_chunk_fires`. **Measurement:** a boundary-split firing
      records a where/why event diagnostic and increments `match_boundary_saves`,
      surfaced in both `ChannelStats` and `ChannelSnapshot` (the how-often). The
      carry dies with the pipeline on Stop/Start (pipelines are rebuilt per run).
- [x] Independent Raw/Display recording config (ADR-013, spec §79 → schema v3).
      `RecordingConfig`/`RecordingMode` split into `RawRecordingConfig` +
      `DisplayRecordingConfig`, each with its own destination/rotation/overwrite/
      timestamps; a channel can run both at once. GUI: Raw panel above Configure
      (live on/off + setup), Display under Configure. Pinned by
      `raw_and_display_recording_run_to_independent_destinations`.
- [x] Inline **`Mark` timestamps** in the live view and `.disp` (§50.2, ADR-016). A
      `Mark { timestamp: Some(..) }` splices the matched pattern's local arrival time
      (before/after, talker-style `TimestampConfig`) into the rendered text via
      `render_text_annotated`; the live viewer rebases the snapshot's `TriggeredMatch`
      onto the scrollback window via its **`view_offset`** (view/scrollback space —
      not `byte_offset`, which counts bytes a paused view skipped; ADR-017) and
      splices the same `MarkRender` text, so display and `.disp` match. `.raw` is
      untouched. A minimal `BytePattern → Mark(+ts)` editor lives under Configure
      (Apply & Restart). Pinned by
      `timestamped_mark_splices_inline_time_into_disp_not_raw`,
      `match_view_offset_tracks_the_paused_view_not_the_raw_stream` + renderer splice
      tests. (On-screen **Highlight** byte-range styling was **dropped** as too
      complex — see ADR-015; ADR-016 explains why the inline timestamp does *not*
      inherit that cost.)
- [ ] (Optional) Draw a glyph for a **bare** `Mark` (no timestamp) in the live stream
      view. A bare `Mark` still only writes the `‹MARK …›` line into `.disp`; the live
      viewer doesn't render a marker for it. Low priority — kept simple deliberately.
- [x] Match-`Record` to a Display/`Both` target: `apply_pending_records` now routes
      `Display`/`Both` through the same lazy begin / clean finalize path as the live
      Display toggle (ADR-012's `set_display_recording`), with spawn-time
      `DisplayRecordingSettings` built from the channel's display-recording config +
      its primary view renderer. Pinned by
      `record_action_display_target_begins_and_stops_display_recording`,
      `record_action_both_target_drives_raw_and_display_together`, and the loopback
      `set_display_recording_toggles_display_recording_live_through_the_orchestrator`.
- [x] `DisplayViewConfig.hex_grouping` schema field (spec §45/§78) — `HexGrouping
      { bytes_per_group, groups_per_line }` added with `#[serde(default)]`, so
      profiles round-trip it (`b8960a1`). Pinned by `hex_grouping_round_trips_through_
      a_profile` + `a_profile_without_hex_grouping_loads_with_the_default`.
- [ ] Consume `hex_grouping` in the Hex renderer: `hex_bytes_per_line` is hardcoded to
      16 in both `build_display_view` (`build.rs`) and the GUI stream renderer
      (`detail.rs`); map the config `HexGrouping` onto `hex_bytes_per_line`/
      `hex_separator` at both sites so the persisted grouping drives the Hex view
      (with a GUI control).
- [x] Live `Record` begin/stop without a restart (ADR-012). `Listener::set_recording`
      → `PipelineRequest::SetRecording` into `run_channel` → the pipeline's lazy
      begin / clean finalize path (shared with the match-rule `Record` action); GUI
      Record/Stop-recording button on the detail pane (`UiCommand::SetRecording`).
      A channel with a destination but `raw_recording.enabled = false` is armed but
      not auto-recording, so the toggle controls it. Pinned by
      `set_recording_begins_and_stops_raw_recording_live` (pipeline) +
      `set_recording_toggles_raw_recording_live_through_the_orchestrator` (loopback).
- [ ] Expose the `.raw` timestamp sidecar in the UI (§57). The byte-offset-keyed
      `.raw.idx` sidecar is **built** (`RawFileRecorder` writes `<offset>,<wall_nanos>`
      when `RawRecordingConfig.timestamp_enabled`), byte-exact-safe (out of `.raw`), and
      kept — but the flag is **config-only**: no GUI/CLI toggle sets it, so it never
      turns on in practice. Add a control (and decide read-side tooling: a companion
      viewer that pairs `.raw.idx` offsets with `.raw` bytes).
- [ ] General match-rule editor UI: the `Idle`/`Record`/`Notify`/`PauseDisplay`
      conditions + actions. The `BytePattern → Mark(+timestamp)` subset now has a
      minimal editor (ADR-016); the rest still arrive only via profiles.
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
- [ ] (YAGNI for now) Diagnostic ordering within one timer tick. `Diagnostic` carries
      only `SystemTime`; on a coarse clock (Windows ~15 ms) two entries can share a
      timestamp and the headline (newest) then orders by severity bucket, not insertion.
      Not observable in practice (lifecycle/recording entries are I/O-separated by ≫ a
      tick). If it ever bites, add a monotonic per-channel sequence to `Diagnostic` and
      sort by it.
- [x] Recording destination uniqueness (ADR-014, spec §6/§55/§71/§121). Two layers:
      (1) **unique Channel Names** — `Profile::validate` flags duplicates
      (`DuplicateChannelName`); GUI add uses a per-kind monotonic, never-reused suffix
      (`UDP_Channel1`…); rename won't commit a duplicate (inline warning); profile load
      rejects duplicate-named channels via the per-channel validation. (2) **advisory
      lock** — `lock_recording_destination` takes std's `File::try_lock` on a `<path>.lock`
      companion (a *sync* std file, so the lock releases deterministically on drop, unlike
      a tokio file), held by `RawFileRecorder`/`DisplayFileRecorder` for the recording's
      lifetime; a conflict is `RecordError::DestinationInUse` → `RecordingFaulted`, channel
      stays Running. An in-process named pre-check at Start was prototyped and removed (it
      faulted the whole channel + showed the bind/port recourse). Tests: name-uniqueness
      validation; recorder lock-conflict; orchestrator two-channel stays-Running.

## Carried over (still valid under v2)

- [ ] Disk-guard GUI exposure (§56.2)
- [ ] **TCP connection channels are currently unobservable** (bigger than the
      recording gap below): each accepted connection runs a full pipeline, but the
      supervisor drops its `PipelineRequest` sender (`runtime/tcp.rs`), so there is
      no per-connection snapshot, stream delta, display, or match evaluation — the
      feature surfaces only connect/disconnect events while paying full pipeline
      cost per connection. UC1 troubleshooting of a TCP feed is impossible today.
      Needs: the supervisor retains per-connection handles (shared registry keyed
      by the minted `ChannelId`), `Listener::snapshot`/`stream_delta` route to
      them, and a GUI decision on how connections appear (sub-tabs under the
      listener channel vs. dynamic top-level channels).
- [ ] Per-connection recording for TCP connection channels (§16.2, deferred §59 naming)
- [ ] RS-422/485 phases (§14.4)
- [ ] CLI parity with GUI for the v2 surface
- [ ] TCP `recv_buffer_bytes` is persisted + specified (§76) but **ignored at runtime**.
      UDP maps it (`build_udp` → `with_recv_buffer`, `build.rs`); `build_tcp_listener`
      passes only the address and `TcpListenerTransport` stores no buffer, so SO_RCVBUF
      is never applied to accepted connections. Wire it through accept (socket2), like UDP.
- [ ] Multi-view pause is only real for the **first** view. There is one shared stream
      buffer; scrollback retention gates on `display_views.first()`'s pause
      (`pipeline.rs`) and the GUI only surfaces the first snapshot view (`detail.rs`).
      A non-primary view can be marked paused but has no per-view stream state to freeze.
      Needs a decision: per-view render state, or document pause as stream-wide (one view).
- [x] `.disp` per-chunk rendering garbled a multi-byte character split across two
      reads, and injected a newline per read chunk — both fixed by the streaming
      renderer (ADR-018): the `.disp` is now the exact rendered stream (no hard
      wraps ever; soft wrap is the viewer's job). Pinned by
      `stream_renderer_is_chunking_invariant` +
      `disp_is_the_exact_rendered_stream_across_read_boundaries`.

## Robustness & performance (external review, 2026-07-11)

- [x] **Recorder faults are now visible on a quiet stream** (§56.1). The recorder
      task publishes its terminal error into a shared fault cell before exiting
      (`Recording::fault_error`); `Recording::state()` reads it eagerly, and
      `ChannelPipeline::check_recording_faults` — run after each ingest *and* on
      `run_channel`'s 250 ms tick — reports it once (error diagnostic carrying
      the cause + `RecordingFaulted`), for Raw and Display recordings alike.
      Previously the handle learned of a write/flush fault only on the *next*
      enqueue, so a stream that went quiet after a disk failure showed Enabled
      forever. Pinned by
      `write_fault_is_visible_on_the_handle_without_further_enqueues` +
      `flush_failure_faults_the_recording`.
- [ ] **Command acks.** A dropped/failed user command is only a tracing warning
      (GUI bridge send failure, `gui/mod.rs`) — Start/Stop/Apply/Record should
      produce an acknowledged result or a visible, persistent error in the GUI.
      Talker has the sibling item (talker TODO).
- [ ] **Match-scanning cost** (behind the workspace benchmark harness — talker
      TODO): `MatchRuleSet::evaluate_stream` scans each rule naively
      (`windows()`), allocates a boundary-carry buffer per chunk, clones actions
      per occurrence, and `note_activity` loops all rules on every chunk to
      re-arm idle ones. Compile the byte patterns into one overlapping
      Aho–Corasick automaton and track idle rules in their own list. Rule counts
      are small today — measure first.
- [ ] **Selected-channel diagnostics are cloned and sorted per poll** (behind
      benchmarks): retained diagnostics are cloned in `ChannelPipeline::snapshot`,
      cloned again crossing the GUI bridge (`gui/bridge.rs`), then sorted every
      poll tick (`gui/state.rs`). Add per-diagnostic sequence numbers and send
      deltas, or consume the snapshot vectors without re-cloning.
- [ ] **One round-trip for the on-screen channel** (behind benchmarks): the GUI
      issues `Snapshot` and `StreamDelta` as separate `PipelineRequest`s each
      poll; combine them into one request per tick.
- [ ] **Doc-drift batch (fold into the next spec/doc pass, no bump alone):**
      removed on-screen Highlight still appears in requirements prose (ADR-015
      dropped it); stale scrollback-size claims vs. the current default; ADR
      text still says the app lacks a dark theme (dark mode landed, Phase 7,
      `66cb1f1`).

## Future work — deferred (spec Appendix A)

- Protocol decoders / field extraction (any return would be a new ADR)
- CSV / JSON / structured-semantic export; session replay
- Plugin architecture; TCP client mode; distributed operation
- Size-based file rotation / old-file pruning / filename templating
- Pre-trigger / pre-match recording capture (§50.2)
- Persistent diagnostic log rotation
- Hard real-time guarantees
