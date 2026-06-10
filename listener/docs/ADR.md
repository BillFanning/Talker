# Architecture Decision Record — Listener

**Crate:** listener
**Status:** Draft (tracks the listener spec)

---

This file is the home for `Listener`'s architecture decisions. Some decisions are
authored inline in the spec where the surrounding context lives — those are listed
here with a pointer to the authoritative section in
[`listener_specification.md`](listener_specification.md) rather than duplicated. Workspace-
and `talker`-level decisions live in [`talker/docs/ADR.md`](../../talker/docs/ADR.md);
`nmea0183` decisions in [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md). Listener
keeps its own ADR numbering (it is a separate crate), so Listener's ADR-001 is *not*
the same as talker's ADR-001.

---

## ADR-001 — Concurrency model: Tokio hybrid

**Authoritative text:** spec §97.1 (with §97.2 blocking-to-async handoff, §99 backpressure, §111 shutdown). Summarized here so the decision is discoverable from the ADR index.

**Decision:** A hybrid runtime. Tokio owns orchestration — command handling, cancellation, bounded queues, fan-out, async-native network I/O, shutdown. Continuous blocking serial receive loops run on dedicated OS threads that own the interface handle and hand data to the async side through bounded `tokio::sync::mpsc` channels (`Sender::blocking_send`). `spawn_blocking` is reserved for bounded, finite operations (open/close, file create/flush, port enumeration) — never for continuous receive loops. Transports are push-based: data-bearing transports emit `ReceivedData`, TCP listeners emit `NewConnection`.

**Contrast with `talker`:** talker deliberately uses **no** async runtime (talker ADR-002 — `std::thread` + `crossbeam-channel`), because it manages a bounded set of *outbound* senders against synchronous `serialport`/`eframe` APIs. Listener faces the opposite shape: many *inbound* network sources, async-native socket I/O, and dynamic TCP-connection fan-in — where Tokio's orchestration earns its keep. The two crates therefore reach opposite conclusions for the same reason (fit the runtime to the I/O shape), and that divergence is intentional.

**Consequences:**
- `blocking_send` backpressure can stall a serial reader → UART/driver overrun, reported as transport-specific loss (§99, §101).
- Continuous blocking loops need a bounded read timeout so they can observe cancellation; shutdown never relies on interrupting an in-progress blocking read (§111).
- Message timing is chunk-granular (`ChunkTime`, §138), not true per-byte hardware timing; per-message timing is derived in the extractor.

---

## ADR-002 — Messages are immutable; decoders are read-only

**Authoritative context:** spec Part on Messages/Decoders (§131–§135, §140).

**Decision:** A `Message` is immutable once emitted by the extractor. Decoders never mutate Messages — they only produce read-only annotations (protocol metadata, integrity metadata). Display formatting never affects what is recorded (Raw Recording is independent of rendering).

**Consequences:**
- Multiple downstream consumers (display, recording, decoding) can share Messages without coordination or copying.
- Raw recording fidelity is guaranteed regardless of decoder or display behavior.

---

## ADR-003 — Profile schema: single monotonic `schema_version`

**Authoritative text:** spec §72.1.

**Decision:** Profiles carry a single monotonically increasing `schema_version: u32`. Each binary knows one current version. Additive changes within that supported schema load via field defaults; a profile newer than the binary is refused. A breaking older schema is refused unless an explicit migration for that schema has been implemented. This mirrors `talker`'s clean-break v2 behavior (talker ADR-013) deliberately, so the two tools share one mental model.

---

## ADR-004 — Single crate, modular internals (resolves OQ-L1)

**Authoritative text:** spec §127–§128 (revised in listener spec v1.1.1).

**Context:** Spec §127 originally sketched a twelve-crate split (`listener-core`, `listener-runtime`, `listener-transport`, `listener-extract`, `listener-decode`, `listener-display`, `listener-record`, `listener-retention`, `listener-config`, `listener-diagnostics`, `listener-cli`, `listener-gui`) and nested `nmea0183` inside `listener`. The crate actually exists as a single `listener` crate alongside `talker` and `nmea0183`.

**Decision:** Keep **one `listener` crate**. The twelve `listener-*` units are realized as modules under `src/` (`core/`, `transport/`, `extract/`, …), preserving the §128 boundaries and dependency direction; only the packaging is collapsed. `nmea0183` is a top-level workspace sibling (shared with `talker`), referenced as an ordinary dependency — not nested. The `lib.rs` + thin-`main.rs` shape follows talker ADR-014, keeping module APIs unit-testable.

**Consequences:**
- §127 is the literal `src/` module map; §128 boundaries are normative whether a unit is a module (now) or a crate (later).
- A later split is non-disruptive: extract a module into a `listener-*` member crate when an external consumer or compile-time concern justifies it. That future split, if taken, gets its own ADR.

## ADR-005 — Delimiter extraction emits a Message at every delimiter, including empty payloads

**Authoritative context:** spec §21 (delimiter extraction) and §150 (the
required "consecutive delimiters" test). The spec mandates the test but does not
prescribe the outcome, so the behavior is fixed here.

**Decision:** `DelimiterExtractor` completes a Message at **every** delimiter
occurrence, even when no payload bytes precede it. Consecutive delimiters
therefore yield empty-payload Messages, and a leading delimiter yields a leading
empty Message — mirroring standard string-split semantics (`"a\n\nb".split` →
`["a", "", "b"]`). When `include_delimiter` is true the "empty" Message still
contains the delimiter bytes; when false its payload is zero-length. Un-terminated
trailing bytes are an incomplete Message and are discarded at `finish` (§112),
not emitted as a final empty Message.

**Rationale:** Listener is a forensic receive-side tool — "bad data is often the
most important data" (§37). Silently collapsing runs of delimiters would hide the
on-wire structure (e.g. stray blank lines, double CRLFs) that an operator is
often looking for. Emit-on-every-delimiter is deterministic (§125), matches the
"extraction defines structure" principle (§5.2), and is the simplest rule to
reason about. A downstream decoder marks an empty/`$`-less Message as it sees fit
(§37); extraction does not pre-judge meaning.

**Consequences:**
- A noisy source can produce many empty Messages; they consume Message Numbers
  and retention slots like any other Message. Bounding pathological cases
  (oversized un-terminated buffers, §119/§124) is a separate config/runtime
  concern, not an extractor responsibility.
- This rule is observable behavior and is pinned by unit tests in
  `extract/delimiter.rs`.

## ADR-006 — Runtime observability & fault-state ownership

**Authoritative context:** spec §94/§101 (transport fault reporting), §136–§137
(`RuntimeCommand` / `RuntimeEvent`), §8/§9 (state machine).

**Context:** A spontaneously-faulting transport (a read/accept error, not a
commanded stop) is detected by a per-channel **fault monitor** — a detached
`tokio` task that owns the transport join handle (ADR-001 / `spawn_monitored_channel`).
The orchestrator (`runtime::Listener`) is **method-based**: it has no background
event loop, so the monitor cannot mutate its state. Before this decision the
monitor only emitted `ChannelFaulted`; `Listener`'s stored `ChannelState` stayed
`Running` until the next command, so `state()` could report `Running` for a
channel whose transport had already died.

Three models were considered:

1. **Event-only (lazy).** The monitor emits `ChannelFaulted`; the stored state is
   reconciled only when the next command runs. Simplest, but `state()` lies and
   command validation can act on a stale state.
2. **Actor / event-loop owner.** Turn the runtime into a task that consumes its
   own `RuntimeEvent` stream and owns all state. Clean single-writer model, but a
   significant architectural commitment for v1.
3. **Shared per-channel state cell.** The monitor flips a lightweight shared flag
   that the orchestrator reads when reporting state and validating commands.

**Decision:** Adopt **model 3**, with a clear division of responsibility:

- **`RuntimeEvent` is authoritative for presentation observers.** The CLI/GUI fold
  the event stream into their own view models; they do **not** read or own
  transport or pipeline state. Richer GUI state is built by folding events (and,
  later, requesting on-demand snapshots), not by owning runtime internals.
- **`Listener` keeps internal channel state only for command validation and
  lifecycle control** (enforcing the legal §9 transitions).
- **Spontaneous transport faults reconcile that internal state through a
  lightweight shared per-channel flag** (`Arc<AtomicBool>`). The fault monitor
  sets the flag *and* emits `ChannelFaulted`; `Listener::state` and command
  validation treat a set flag as `Faulted` (an "effective state" overlaid on the
  stored lifecycle state). A fresh flag is installed at each `start`; `stop`
  clears it (`Faulted → Stopped`, §8.5). No actor/event-loop rewrite.

We deliberately do **not** adopt model 2 yet. If GUI requirements prove that
events plus on-demand snapshots are insufficient — e.g. the runtime must *push*
rich incremental state — revisit with a new ADR; the shared-flag design leaves a
clean path and does not foreclose it.

**Consequences:**
- `state()` is accurate immediately after a spontaneous fault, without a
  background runtime loop.
- The stored `ChannelState` plus the fault flag together form the *effective*
  state; everything user-facing (`state`, `start`/`stop` validation, `shutdown`)
  reads the effective state.
- Reconciliation covers **data channels** (serial/UDP) via the fault monitor. A
  TCP **listener-acceptor** fault is not reconciled through this flag in v1 (its
  supervisor reports per-connection faults as events); revisit if acceptor faults
  need to surface as listener state.
- Live readout of *retained* messages / diagnostics / decoded metadata while a
  channel runs still requires an on-demand snapshot API — separate, upcoming work
  (the observability surface), not covered here.
- Pinned by tests: `runtime::channel` (`is_faulted` after a fault) and
  `runtime::listener` (`state()` reconciles to `Faulted`, then clears on `stop`).

## ADR-007 — Transport data-loss observability boundary

**Authoritative context:** spec §99 (backpressure matrix), §100 (reception
priority), §101 (data-loss reporting), §97.1 / ADR-001 (the blocking serial
reader), §56.1 (recorder fault), §137 (the fixed `RuntimeEvent` set).

**Context:** §101 requires Listener to report discarded data (channel, time,
overflow type, *estimated loss where practical*) but is explicit that some loss
"is not reliably observable from userland; Listener reports the loss it can
detect rather than guaranteeing detection of all loss." We need a precise,
honest line between what we report and what we cannot — and to avoid both silent
loss and fabricated loss counts.

**Decision:** Classify transport-relevant loss into three tiers and fix the
reporting contract for each.

1. **Detected and reported, with a known truncation point — recorder overflow
   (§56.1).** Under sustained overload Listener preserves reception and *faults*
   the affected recording rather than stalling the reader or writing a gapped
   file. The recorder emits `RecordingFaulted(ChannelId)` and records the
   truncation point; the artifact is contiguous and byte-exact up to a known end.
   *Status: implemented and tested* (`recording_overflow_faults_emits_event_and_reception_continues`).

2. **Detectable as an event, not quantifiable — a sustained reader stall on the
   Transport→Extractor edge (§97.1, §99).** This is the only edge permitted to
   backpressure the reader. In process it *stalls, never drops* — zero loss
   inside our queues. But a stall long enough lets the OS/UART receive buffer
   overrun, losing bytes *upstream of us*. We can detect the stall; we cannot
   count the lost bytes (no portable userland signal for UART/driver overrun).
   **Contract:** the serial reader watches `blocking_send`; a stall beyond a
   heuristic threshold (`STALL_WARNING`, 250 ms) sends a
   `TransportNotice::ReceptionStalled { channel_id, stalled_for }` once per stall
   episode — self-describing, carrying the observed stall **duration** (the honest
   §101 proxy) and **no fabricated byte count**. The pipeline records it as a
   Warning `Diagnostic` and emits `ReceptionStalled` (see the seam below). Momentary
   backpressure that drains quickly is normal and must not notify.
   *Status: implemented and tested* (`sustained_stall_sends_a_reception_stalled_notice`,
   `momentary_backpressure_sends_no_notice`, `run_channel_records_a_transport_notice_as_a_diagnostic`).

3. **Fundamentally unobservable — pre-receive kernel/NIC loss.** Kernel-dropped
   UDP datagrams, and any bytes lost in the driver/NIC before our `recv`, leave
   no reliable userland signal. Listener **does not report** what it cannot
   detect and **does not fabricate** loss for data it never saw: it numbers and
   reports exactly what it received (§24). Quantifying this is out of scope for
   v1; reducing it is an operational concern (socket buffer sizing), not a
   reporting one.

**Event signal — `WarningRaised` then `ReceptionStalled` (superseded by spec
v1.2).** When this ADR was written the `RuntimeEvent` set was fixed (spec §137), so
a reader stall reused `WarningRaised` — a §93 Warning — rather than a new variant
needing sign-off. **Spec v1.2 added a dedicated `ReceptionStalled(ChannelId,
Duration)` event** (§137) and marked the enum `#[non_exhaustive]`, so the overload
is retired: the stall now emits `ReceptionStalled` carrying the duration. The
retained §95 `Diagnostic` remains the **rich detail surface** (channel, time,
human text); the event is the lightweight signal. *Implemented:* the seam emits
`ReceptionStalled`, and both `RuntimeEvent`/`RuntimeCommand` are `#[non_exhaustive]`.

**The transport→diagnostics seam (implements tier 2's record).** A transport states
*what happened* via a `TransportNotice` (in `transport/`, with no dependency on the
diagnostics/event vocabulary); the **pipeline** — the channel's `DiagnosticLog`
owner — decides how it is recorded and reported. `run_channel` drains a bounded
`Receiver<TransportNotice>` in its `select!` (alongside ingest and snapshot
requests) and calls `record_notice`, which writes a Warning `Diagnostic` (naming
the channel and stall duration) **and** emits the spec-appropriate §137 event —
`ReceptionStalled` (carrying the duration) under spec v1.2 — keeping the §95 record
and the §137 event paired in one owner. The notice channel is created by the
orchestrator; only the serial transport is given the sender (`with_notice_sender`),
because only serial can stall the reader (§97.1). The §95 record is now visible in
a live `ChannelSnapshot.diagnostics`, which is what a GUI reads.

Why this shape (vs. the alternatives): the transport must not depend on
`diagnostics`/`RuntimeEvent` (§128 layering), so it emits a transport-local notice,
not a `Diagnostic`. The notice is **self-describing** (carries its `channel_id`) so
the pipeline formats and routes it without assuming which channel a notice is for.
The `run()` transport contract is left unchanged — UDP/TCP are async and never
stall the reader, so burdening every transport with a notice sender would imply a
capability they don't have; serial attaches it via a builder instead.

The notice channel is **advisory and bounded** (`TRANSPORT_NOTICES`, 16): the
sender uses `try_send` and drops on full, named explicitly because the dropped item
is itself a loss-warning — we accept losing a warning under overload, but we never
block the reader to keep one (that would cause the stall it warns of). A
dropped-notice counter can be added later only if the drop rate proves to matter.

**Consequences:**
- `WarningRaised` was shared with recording-enable failures (§55), so a UI could
  not distinguish them from the event alone. Spec v1.2's dedicated
  `ReceptionStalled` event removes that ambiguity for stalls; the retained
  `Diagnostic` text still carries the rich detail (channel, duration).
- UDP/TCP backpressure does not stall an OS thread (async `send().await`), so
  there is no serial-style stall notice; sustained UDP backpressure manifests as
  tier 3 (kernel drops) and is unreportable by design.
- `TransportNotice` is `#[non_exhaustive]`: future non-terminal transport
  conditions (e.g. a recoverable read hiccup) extend it without a breaking change.

## ADR-008 — GUI↔runtime bridge: a background driver task owns the `Listener`

**Status:** Accepted. **Context:** spec §3 (thin presentation layers), §136/§137
(command/event vocabulary), §10 (observable state); AGENTS §5 ("UI threads never
perform I/O and never block"); ADR-001 (Tokio hybrid) and ADR-006 (events are the
authoritative push surface, snapshots the pull surface).

**Problem.** egui/eframe is a *synchronous, main-thread, immediate-mode* loop
(`eframe::run_native` owns the OS event loop and calls `App::update` per frame). The
`Listener` orchestrator is *async* (`async fn start/stop/snapshot/reconnect_tick/…`).
Calling those from inside `update` via `Runtime::block_on` would block the UI thread
on every interaction — exactly what AGENTS §5 forbids. So the GUI cannot own the
`Listener` directly.

**Decision.** Put a **background "driver" task** between the egui App and the
`Listener`, mirroring talker's UI↔talker-thread command/status split (AGENTS §5)
adapted to listener's Tokio model:

- A dedicated Tokio runtime (its own thread) **owns the `Listener`**. It runs a loop
  that: drains a **command channel** (GUI → driver) and calls the matching `Listener`
  method; forwards the `Listener`'s `RuntimeEvent` stream to the GUI; and on a timer
  (a few Hz) polls `snapshot(id)` for each running Channel and pushes the result to
  the GUI. It also drives `reconnect_tick` (the loop the CLI already runs).
- Two message types cross the boundary:
  - `UiCommand` (GUI → driver): `Start/Stop/ApplyPending/AddChannel/EnableRecording/
    PauseDisplay/ResumeDisplay/SetRts/SetDtr/SetMatchRuleEnabled/MarkNow/Shutdown`.
    This is the path that finally needs a command channel **into the running
    pipeline** for the dynamic §165/§161 actions (live rule-toggle, MarkNow,
    mid-run recording) — see the deferred items; building it is part of this work.
  - `UiUpdate` (driver → GUI): `Event(RuntimeEvent)` and
    `Snapshot(ChannelId, ChannelSnapshot)`.
- The driver holds a clone of `egui::Context` and calls `request_repaint()` when it
  pushes an update, so a streaming source wakes the UI without the App busy-polling.
- The egui App is **pure presentation**: it folds `UiUpdate`s into a testable
  per-Channel view-model (`AppState::apply`), lays out widgets reading that model,
  and emits `UiCommand`s on interaction. No `Listener`, no I/O, no `block_on`.

**Why not the alternatives.**
- *`block_on` in `update`* — violates AGENTS §5 (UI blocks on orchestration and on
  every async snapshot); also fights egui's frame budget.
- *GUI owns the `Listener` directly* — impossible cleanly: the methods are `async`
  and take `&mut self`; the immediate-mode loop has nowhere to `.await`.
- *Snapshot-on-request from the UI thread* — would still block or require an async
  round-trip per frame; the driver's timer-push keeps the UI reading owned, already
  -current view-models (ADR-006's pull surface, fetched off the UI thread).

**Consequences.**
- The business logic (command handling, view-model fold) lives in testable structs;
  the `gui/` egui code stays thin (AGENTS §5) and is exercised only by eye.
- Channels carry **owned** snapshots/events, so the UI never shares mutable pipeline
  state and a slow UI can never stall reception (the driver's pushes are advisory,
  `try_send`/drop-newest like the other observer edges, §99).
- This bridge is also where the `RuntimeCommand` enum finally gets aligned with §136
  and dispatched for real (today it is vestigial — commands are direct `Listener`
  methods); and where the missing **command channel into `run_channel`** is built,
  unblocking the deferred §165 live actions.
- GUI-only state (window geometry, last layout) uses eframe's built-in persistence,
  never the profile schema (mirrors the talker rule).

**Build order (small, reversible first):** (1) deps + `--gui` dispatch + a minimal
window [this step]; (2) the driver + `UiCommand`/`UiUpdate` + a pure `AppState`
reducer, unit-tested, no egui; (3) a one-Channel vertical slice (list + start/stop +
a live snapshot pane); (4) breadth by mapping snapshot fields to panes.

---

## ADR-009 — The live viewer is fed from the verbatim pre-extraction byte stream (`DisplaySource::Stream`)

**Status:** Accepted. **Context:** spec §17 (per-Channel Stream vs Message Mode),
§18 (Stream Mode: continuous bytes, no Message Numbers/timestamps, "displayed as a
stream"), §41 (`DisplaySource { Stream, Messages }` — a Display View operates on
either stream data or completed Messages), §24 (in Message Mode with delimiter
exclusion, CRLF "need not remain in the Message payload"), §53/§142 (the
pre-extraction chunk path, `write_chunk` — bytes "exactly as received"), §99.1
(non-blocking distribution order), §88 (retention is Message-**count** based);
ADR-006 (snapshots are the pull surface) and ADR-008 (the GUI↔runtime bridge).

**Problem.** The live viewer was wired only to the **Messages** display source —
extracted, numbered, decoded frames. That is the wrong source for a wire
troubleshooting view, three ways:
- **Delimiter extraction strips the terminator** (§24). For NMEA the `\r\n` that
  *defines* each sentence is consumed by the extractor, so the viewer literally
  cannot show what is on the wire.
- **Stream-Mode channels produce no Messages at all** (`StreamExtractor` emits
  nothing — the generic serial default), so their viewer is empty.
- **Chunk boundaries are arbitrary.** Non-UDP reads return "whatever the OS has,
  when it has it" — capped at a 4 KB (serial) / 8 KB (TCP) buffer, with serial also
  returning empty on a 100 ms cancellation timeout. A read can split a sentence
  anywhere; boundaries track OS buffering and timing, never content. So a Messages-
  or chunk-segmented viewer diverges between serial and UDP for the same data.

The spec already defines the right source — `DisplaySource::Stream` (§18/§41), the
verbatim pre-extraction bytes — but it was never surfaced: the chunk path (§53/§142)
fed only raw *recording*, not the viewer.

**Decision.** Feed the live viewer from the **verbatim pre-extraction byte stream**.
Maintain a bounded per-Channel byte ring, appended in `Pipeline::ingest` *before*
extraction — a second non-blocking pre-extraction tap alongside the raw recorder
(§53/§99.1), **independent of the Channel's extraction config**. Surface its tail in
`ChannelSnapshot` (ADR-006 pull surface), cloned only for the on-screen Channel. The
GUI renders the tail as one continuous, selectable view through the existing
`DisplayView` renderer: Rendered honors the data's real CR/LF (terminal semantics,
§44), Hex is a bytes-per-line dump, Raw shows control pictures. Concatenating chunks
in arrival order reconstructs the exact wire stream regardless of read-chunk
boundaries — the reassembly an extractor must do across buffers (§21) is unnecessary
here because nothing is reframed. This makes serial and UDP behave identically and
gives **every** Channel a working viewer.

The work is phased. Phase 1: the verbatim Stream viewer becomes the default — and,
for now, only — viewer. Phase 2: the per-view `DisplaySource` **Stream ↔ Messages**
switch (§41), which restores the Messages-source view (decoded, numbered,
`[type]`-tagged, match-highlighted). Until then the Messages machinery keeps running
for decoding, Match Rules, diagnostics, and message-framed recording — it is simply
not rendered.

**Why not the alternatives.**
- *Render from extracted Messages (the prior direction).* Loses the delimiters under
  delimiter extraction (§24), shows nothing under Stream extraction, and segments on
  arbitrary frame/chunk boundaries — none of which is the wire.
- *Concatenate the Messages display history into a pseudo-stream (an interim hack).*
  Same source defect: CRLF-stripped frames run together with no breaks. Wrong layer.
- *Reuse the raw-recording path.* That writes to disk; the viewer needs an in-memory,
  bounded, snapshot-cloneable tail. Same data, different sink.

**Consequences.**
- A new bounded byte ring per Channel, capped by **bytes** (~128 KB default) — there
  are no Message boundaries to count, so §88's count-based retention does not apply.
  Trivial CPU/memory; always-on even for Stream-Mode channels.
- Stream Mode has no Message Numbers or timestamps (§18): the Stream viewer drops the
  per-message prefix (timestamp / # / `[type]`), and "Recent messages (N)" becomes a
  byte count. Match-rule highlighting (message-number-keyed, §50.2/§165) and
  Display-View pause move to the deferred Messages view.
- The snapshot grows by the tail clone (≤ cap), polled at the ADR-006 cadence (5 Hz)
  for the selected Channel only — negligible.
- The recently built Messages-based viewer (line-virtualized per-message log) is set
  aside, to return behind the §41 source switch.

**Build order (small, reversible first):** (1) byte ring + `ChannelSnapshot` field +
GUI render + revert the interim hack [this step]; (2) the per-view
`DisplaySource::Stream/Messages` switch (§41), restoring the Messages view; (3)
Stream-view pause and optional byte/line gutters.

## Open questions

_None open. (OQ-L1 resolved by ADR-004 above.)_
