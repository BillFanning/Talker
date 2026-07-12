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

**Decision:** A hybrid runtime. Tokio owns orchestration — command handling, cancellation, bounded queues, fan-out, async-native network I/O, shutdown. Continuous blocking serial receive loops run on dedicated OS threads that own the interface handle and hand data to the async side through bounded `tokio::sync::mpsc` channels (`try_send`, with a bounded retry loop while the queue is full — see ADR-007's refinement: the retry loop lets the stall notice fire mid-stall and observes cancellation). `spawn_blocking` is reserved for bounded, finite operations (open/close, file create/flush, port enumeration) — never for continuous receive loops. Transports are push-based: data-bearing transports emit `ReceivedData`, TCP listeners emit `NewConnection`.

**Contrast with `talker`:** talker deliberately uses **no** async runtime (talker ADR-002 — `std::thread` + `crossbeam-channel`), because it manages a bounded set of *outbound* senders against synchronous `serialport`/`eframe` APIs. Listener faces the opposite shape: many *inbound* network sources, async-native socket I/O, and dynamic TCP-connection fan-in — where Tokio's orchestration earns its keep. The two crates therefore reach opposite conclusions for the same reason (fit the runtime to the I/O shape), and that divergence is intentional.

**Consequences:**
- `blocking_send` backpressure can stall a serial reader → UART/driver overrun, reported as transport-specific loss (§99, §101).
- Continuous blocking loops need a bounded read timeout so they can observe cancellation; shutdown never relies on interrupting an in-progress blocking read (§111).
- Timing is chunk-granular (`ChunkTime`, §138), not true per-byte hardware timing; recording timestamps and liveness derive from it (§26, §57, §166). *(Originally "per-message timing in the extractor" — both removed by ADR-010.)*

---

## ADR-002 — Messages are immutable; decoders are read-only

> **Superseded by ADR-010 (spec v2.0):** Messages and decoders are removed. The
> surviving principles live on as §103 (received chunks are immutable, shared via
> `Arc`) and §5.4/§5.5 (display/recording never alter received bytes).

**Authoritative context:** spec Part on Messages/Decoders (§131–§135, §140; removed in v2.0).

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

**Decision:** Keep **one `listener` crate**. The twelve `listener-*` units are realized as modules under `src/` (`core/`, `transport/`, `display/`, `record/`, `runtime/`, …; `extract/` and `decode/` were later removed by ADR-010), preserving the §128 boundaries and dependency direction; only the packaging is collapsed. `nmea0183` is a top-level workspace sibling (shared with `talker`), referenced as an ordinary dependency — not nested. The `lib.rs` + thin-`main.rs` shape follows talker ADR-014, keeping module APIs unit-testable.

**Consequences:**
- §127 is the literal `src/` module map; §128 boundaries are normative whether a unit is a module (now) or a crate (later).
- A later split is non-disruptive: extract a module into a `listener-*` member crate when an external consumer or compile-time concern justifies it. That future split, if taken, gets its own ADR.

## ADR-005 — Delimiter extraction emits a Message at every delimiter, including empty payloads

> **Obsolete under ADR-010 (spec v2.0):** delimiter extraction is removed entirely;
> there is nothing to emit. Kept for historical context only.

**Authoritative context:** spec §21 (delimiter extraction; removed in v2.0) and §150 (the
required "consecutive delimiters" test). The spec mandated the test but did not
prescribe the outcome, so the behavior was fixed here.

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

**Authoritative context:** spec §94/§101 (transport fault reporting), §137
(`RuntimeEvent`; the §136 `RuntimeCommand` enum was later removed by ADR-012 — the
command surface is the `Listener` method API), §8/§9 (state machine).

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

- **`RuntimeEvent` is the presentation surface, but advisory — not a guaranteed
  ledger.** The CLI/GUI fold the event stream into their own view models; they do
  **not** read or own transport or pipeline state. Events are sent non-blocking
  (`try_send`) so a slow/saturated observer can never stall the runtime — which means
  an event *may be dropped* under saturation. The design tolerates this because the
  **truth lives elsewhere and is re-derivable**: durable facts (diagnostics, recording
  state, liveness, the effective channel state) live in the periodically-polled
  **snapshot**, so a dropped event self-corrects within one poll (≤200 ms at 5 Hz).
  Events are best read as *wake-up / status hints* that make the UI feel live between
  polls, **not** as the authoritative record. (If a future requirement needs
  exactly-once lifecycle delivery, split critical lifecycle events onto an awaitable
  channel — see model 2 — rather than treat the advisory stream as a ledger.)
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
- Live readout of retained content (the stream scrollback / diagnostics under
  ADR-010; originally retained messages / decoded metadata) while a channel runs
  still requires the on-demand snapshot API — built since, and the message-era
  parts of this ADR are superseded by ADR-010.
- **Diagnostics are runtime-written, GUI-rendered (single writer).** Every diagnostic
  is recorded once in the pipeline's `DiagnosticLog` with a real timestamp; the GUI
  never synthesizes entries — it renders the `ChannelSnapshot.diagnostics` it polls.
  Three delivery details follow from the method-based (no event-loop) runtime: (a) at
  **stop** the orchestrator takes one *final* snapshot of the returned, already-finalized
  pipeline (the 5 Hz poll can't fire during the synchronous stop) so the stop-time
  Channel-Stopped/Recording-Stopped events reach the GUI; (b) a Channel's diagnostics are
  **retained across its own stop→start** (spec §89.1) in a per-channel `retained_diagnostics`
  vec, served via a minimal snapshot while not running and replayed (`DiagnosticLog::seed`)
  into the next pipeline; (c) a **start fault** that never runs a pipeline is appended to
  that vec as an Error diagnostic so it shows and survives a restart. Pinned by
  `diagnostics_are_retained_across_a_stop_start_cycle`,
  `a_start_fault_is_retained_as_an_error_diagnostic`, and the loopback final-snapshot test.
- Pinned by tests: `runtime::channel` (`is_faulted` after a fault) and
  `runtime::listener` (`state()` reconciles to `Faulted`, then clears on `stop`).

**Correction (2026-07-12).** The claim above that "the effective channel state
live[s] in the periodically-polled snapshot" was **aspirational, not true as
built**: until now neither `ChannelStats` nor `ChannelSnapshot` carried a
`ChannelState`, and both lanes went silent (`None`) for a non-running channel —
so a dropped lifecycle event did *not* self-correct; the GUI row stayed stale
(external review round 2, High). Now made true as written: both structs carry
`state` (the orchestrator stamps `effective_state()` over the pipeline's
placeholder — lifecycle belongs to the orchestrator, not the pipeline) plus
`reconnect_pending` (so a polled `Faulted` distinguishes "still retrying" =
Reconnecting from "gave up / not retrying" = Faulted without depending on the
advisory reconnect events), and `snapshot`/`channel_stats` always serve for a
known channel — live when running, synthesized from the retained
diagnostics/activity otherwise. The GUI reducer reconciles its derived status
from every polled update, leaving the transitional `Starting`/`Stopping` states
to settle on their own. Pinned by
`a_dropped_lifecycle_event_self_corrects_on_the_next_poll`,
`polled_fault_with_reconnect_pending_reads_reconnecting`, and
`transitional_polled_states_do_not_flap_the_row`.

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
   **Contract:** the serial reader watches its send to the full queue; a stall
   beyond a heuristic threshold (`STALL_WARNING`, 250 ms) sends a
   `TransportNotice::ReceptionStalled { channel_id, stalled_for }` once per stall
   episode — self-describing, carrying the observed stall **duration** (the honest
   §101 proxy) and **no fabricated byte count**. The pipeline records it as a
   Warning `Diagnostic` and emits `ReceptionStalled` (see the seam below). Momentary
   backpressure that drains quickly is normal and must not notify. *(Refined: the
   stall is a bounded retry loop, not a parked `blocking_send`, so the notice is
   raised **while the stall is ongoing** — a permanently wedged pipeline is not
   silent — and cancellation is observed mid-stall, §111.)*
   *Status: implemented and tested* (`sustained_stall_sends_a_reception_stalled_notice`,
   `a_wedged_pipeline_raises_the_notice_while_still_stalled_and_can_cancel`,
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
`ReceptionStalled`, and `RuntimeEvent` is `#[non_exhaustive]`. *(The sibling
`RuntimeCommand` enum mentioned in the original was removed by ADR-012.)*

**The transport→diagnostics seam (implements tier 2's record).** A transport states
*what happened* via a `TransportNotice` (in `transport/`, with no dependency on the
diagnostics/event vocabulary); the **pipeline** — the channel's `DiagnosticLog`
owner — decides how it is recorded and reported. `run_channel` drains a bounded
`Receiver<TransportNotice>` in its `select!` (alongside ingest and snapshot
requests) and calls `record_notice`, which writes a Warning `Diagnostic` (naming
the channel and stall duration) **and** emits the spec-appropriate §137 event —
`ReceptionStalled` (carrying the duration) under spec v1.2 — keeping the §95 record
and the §137 event paired in one owner. The notice channel is created for every
data channel. Only the live serial reader receives a sender clone
(`with_notice_sender`), because only serial can stall the reader (§97.1); every
runtime monitor retains the original sender for the terminal-fault contract added
by ADR-020. The §95 record is visible in `ChannelSnapshot.diagnostics`, which is
what a GUI reads.

Why this shape (vs. the alternatives): the transport must not depend on
`diagnostics`/`RuntimeEvent` (§128 layering), so it emits a transport-local notice,
not a `Diagnostic`. The notice is **self-describing** (carries its `channel_id`) so
the pipeline formats and routes it without assuming which channel a notice is for.
The `run()` transport contract is left unchanged — UDP/TCP are async and never
stall the reader, so their transport implementations do not receive a notice
sender; serial attaches one via a builder. Terminal outcomes are joined and
reported by runtime monitors outside that transport contract (ADR-020).

The live-stall notice path is **advisory and bounded** (`TRANSPORT_NOTICES`, 16):
the serial reader uses `try_send` and drops on full, named explicitly because the
dropped item is itself a loss-warning — we accept losing a warning under overload,
but never block the reader to keep one (that would cause the stall it warns of).
A dropped-advisory counter can be added later only if the drop rate proves to
matter. Terminal fault causes later gained a distinct delivery contract in
ADR-020: they share the bounded queue but await capacity after reception ends.

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
- The GUI's `UiCommand` is the command surface from the App's side; the driver
  translates it into direct `Listener` async method calls. _(Superseded in part by
  ADR-012: there is no separate `core::RuntimeCommand` enum to "align with §136" — the
  method API is the command surface. The missing **command channel into `run_channel`**
  is still the seam that unblocks the deferred §165 live actions, built as `Listener`
  methods + an internal pipeline command, not a top-level command enum.)_
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
  for the selected Channel only. _(Superseded by ADR-011: at the eventual ~1 MB cap
  this 5 Hz full-buffer clone was **not** negligible — it is replaced by incremental
  `StreamDelta` fetches, and the snapshot no longer carries the tail.)_
- The recently built Messages-based viewer (line-virtualized per-message log) is set
  aside, to return behind the §41 source switch.

**Build order (small, reversible first):** (1) byte ring + `ChannelSnapshot` field +
GUI render + revert the interim hack [this step]; (2) the per-view
`DisplaySource::Stream/Messages` switch (§41), restoring the Messages view; (3)
Stream-view pause and optional byte/line gutters.

> **Superseded by ADR-010 (spec v2.0).** ADR-009 phases 1–2 shipped, but v2.0 then
> removed the Message half entirely, so the phase-2 "Messages source" is gone. The
> Stream viewer ADR-009 introduced is now the *only* viewer.

---

## ADR-010 — Stream-only architecture: the Message infrastructure is removed (spec v2.0)

**Status:** Accepted. **Context:** spec **v2.0** (revision note + §17–18, §40–46, §50.2, §51–59), which supersedes the Message-mode half of v1.x. ADR-009 (the Stream display source) was the first step; v2.0 finishes the trajectory by removing the Message half rather than keeping both.

**Decision.** `Listener` is a pure **stream** tool. Received bytes are one verbatim stream that is displayed (Raw/Rendered/Hex), searched (Find & Triggers), and recorded (Raw `.raw` + Display `.disp`). Removed: Message Mode, Message Extraction (delimiter / fixed-length / protocol), Message Numbering, decoders + the `nmea0183` dependency, integrity metadata, message-framed recording (`.ssdat`) + subsampling, the Messages display source, and message-keyed Match conditions. Find & Triggers re-root on the byte stream — `BytePattern` (cross-chunk scan) + `Idle` conditions; `Highlight` (byte range), `Record`, `Notify`, `Mark` actions anchored on **byte offset**. *(The `Highlight` action was later removed — see ADR-015.)*

**Why.** The actual use (single-stream troubleshooting; long-run multi-channel logging) is stream-centric. The extraction→decode→message spine added surface (framing config, decode config, numbering, `.ssdat`) the workflow never used, and made the wire view harder to keep faithful (delimiter extraction strips the very bytes that define a sentence). A stream-only tool is simpler, smaller, and a better fit; `nmea0183` lives on for `talker`.

**Consequences.**
- `extract/` and `decode/` modules are removed; `listener` drops its `nmea0183` dependency.
- The pipeline collapses to: transport → bounded pipeline queue → non-blocking fan-out (raw recorder, display/scrollback, display recorder, find/triggers, diagnostics). The single backpressure edge (§99) is now **Transport→Pipeline**.
- `ChannelConfig` loses `extraction` and `decoder`; `RecordingConfig` loses `subsample`; `DisplayViewConfig` loses `source`/`annotations`/`subsample`/`timestamp`; `RetentionConfig` is byte-based. `schema_version` bumps (breaking; v1 profiles refused).
- Recording extensions: **`.raw`** (was `.dat`) and `.disp`; `.ssdat` gone.
- The GUI loses the framing selector, decode toggle, Messages-source switch, and per-message toggles (some only just built); the Stream viewer, byte-based liveness, recording UI, and pause all stay.
- Reversible in git; low-regret — the message path was unused in the workflow.

**Build order.** (1) spec rewrite to v2.0 [done]; (2) strip the runtime (remove `extract/`/`decode/`, collapse the pipeline, byte-based retention); (3) trim the config schema + GUI; (4) re-root Find & Triggers on the stream; (5) test cleanup.

## ADR-011 — Live stream delivery is incremental, not bundled in the snapshot

**Status:** Accepted. **Context:** spec §87 (bounded stream scrollback), §100 (reception priority / non-blocking observers), §166 (liveness); ADR-006 (snapshots are the pull surface), ADR-008 (the GUI↔runtime driver polls at ~5 Hz), ADR-009 (the live viewer is fed from the verbatim stream).

**Problem.** ADR-009's `ChannelSnapshot` bundled the whole stream scrollback (`stream_tail: Arc<[u8]>`, capped at the byte-retention limit — ~1 MB by default). The ADR-008 driver polls the selected channel's snapshot at 5 Hz, and the GUI re-rendered the tail into one selectable `egui::Label` each frame. Both costs scaled with the *buffer*, not with new data: at 5 Hz a full-buffer clone shipped continuously, and a non-virtualized ~1 MB selectable label stalled the UI. The symptom (reported) was the GUI becoming unresponsive after a steady low-rate source had run long enough to fill the scrollback (~minutes) — confirming the cost was buffer-fill, not throughput. This is the opposite of what a high-throughput acquisition tool needs.

**Decision.** Split the pull surface so nothing is O(buffer) in steady state:
- The **snapshot carries only the small, bounded observable state** — diagnostics, recent match firings, view pause, recording state, liveness, and the stream's `stream_end_offset` (a cursor target). It no longer carries the scrollback bytes.
- The scrollback is read **incrementally** through a new `PipelineRequest::StreamDelta { since }` → `StreamDelta { base_offset, bytes, end_offset }`: only the bytes at/after the consumer's absolute cursor. A cursor behind the (bounded) retained window returns the whole window with `base_offset > since` — a **reset** signal, not an append. The pipeline tracks `stream_dropped` (bytes evicted from the front) so an absolute offset locates a byte in (or past) the ring in O(returned bytes).
- The **driver** holds a per-selected-channel cursor, polls `stream_delta` alongside the snapshot, pushes only non-empty deltas (a "caught up" empty delta is a no-op, so the UI isn't woken for nothing), and resets the cursor to 0 on `Select` and on `ChannelStarted`/`ChannelReconnected` (a (re)start resets the runtime's stream offset to 0).
- The **GUI** accumulates delta bytes per channel (capped, with eviction/restart reset), memoizes the line-split render keyed on the cursor + view mode, and **virtualizes the layout** with `ScrollArea::show_rows` so only visible rows are laid out.

End to end the steady-state cost is now: ingest O(chunk), snapshot O(small bounded state), stream delta O(new bytes), GUI render/layout O(new bytes)/O(visible rows). Nothing re-touches the whole buffer.

**Why not the alternatives.**
- *Keep the tail in the snapshot but cap the rendered region.* Still clones the (capped) region every poll and re-renders a fixed slab each frame — O(slab), not O(new); and it drops scrollback-to-start from the live view. Incremental is strictly cheaper and keeps the full window.
- *Diff the tail in the GUI against the last snapshot.* The expensive clone (snapshot→GUI, 5 Hz) would remain; only the render would be saved. The waste is on the wire, so the fix belongs at the request boundary.
- *Virtualization alone.* Fixes the layout stall but leaves the 5 Hz full-buffer clone. Necessary but not sufficient; we do both.

**Consequences.**
- `ChannelSnapshot.stream_tail` is removed; `stream_end_offset` replaces it. Tests that asserted verbatim bytes now fetch via `stream_delta` (a `#[cfg(test)]` `stream_tail()` accessor remains on the pipeline for unit tests).
- A new `UiUpdate::StreamDelta` rides beside `Snapshot`/`Stats`; the App folds it into per-channel accumulated bytes.
- The reset-on-eviction contract (`base_offset > since`) is the consumer's signal to re-seed rather than append; restart is handled by resetting the cursor (offsets restart at 0).
- This refines ADR-006's pull surface exactly along the "push rich incremental state" axis that ADR-006 left open; no actor/event-loop rewrite was needed.
- Pinned by tests: `pipeline` (`stream_delta_serves_only_new_bytes_since_a_cursor`, `stream_delta_resets_when_the_cursor_was_evicted`), `gui::state` (`stream_deltas_accumulate_incrementally`, `stream_delta_reset_on_eviction_replaces_rather_than_appends`, `restart_clears_accumulated_stream`).

## ADR-012 — The command surface is the `Listener` method API; no `RuntimeCommand` enum

**Status:** Accepted. **Context:** spec §136 (Runtime Commands), §3 (UI owns no runtime state), ADR-006 (events are the push surface; commands are direct method calls), ADR-008 (the GUI↔runtime driver translates `UiCommand` into `Listener` calls).

**Problem.** `core::RuntimeCommand` was defined (spec §136) as the UI→runtime command vocabulary, mirroring `RuntimeEvent`. But it was never constructed or matched anywhere — ADR-008 itself called it "vestigial … commands are direct `Listener` methods," anticipating a future where the GUI bridge would "finally align it with §136 and dispatch it for real." That future did not arrive, and the architecture that *did* land makes it redundant:

- The runtime exposes commands as **async `Listener` methods** (`start`/`stop`/`apply_pending`/`pause_display`/`set_rts`/…). They take `&mut self` and `.await`; this is the real, tested command API used by both the CLI and the GUI driver.
- The GUI has its **own** `UiCommand` enum (`gui::bridge`) — its on-the-wire form across the App↔driver channel — which the driver translates into those method calls. `UiCommand` is already *richer* than `RuntimeCommand` ever was (`AddChannel`, `RemoveChannel`, `Rename`, `Reconfigure`, `Select`, `SaveProfile`/`LoadProfile`), so `RuntimeCommand` is not even a superset to grow into.

A second, parallel command enum on top of a working method API + a GUI transport enum is a layer with no callers — exactly the kind of spec-vs-code drift the project guards against.

**Decision.** Remove `core::RuntimeCommand`. The **command surface is the `Listener` async method API**; the GUI's `UiCommand` is the presentation-layer transport the driver maps onto it (ADR-008). `RuntimeEvent` is unaffected — it is genuinely the push surface (ADR-006) and stays in `core::command`.

**Why not the alternatives.**
- *Keep the enum and wire it for real (the original §136 intent).* Would add a dispatch layer parallel to the working method API and the GUI's `UiCommand`, for no capability gain — two enums and a method API all expressing the same operations. The deferred live actions (`SetMatchRuleEnabled`, `MarkNow`, mid-run record toggle) need a **command channel into `run_channel`** (ADR-008), not a top-level orchestrator enum; that seam is where they will land, as new `Listener` methods + an internal pipeline command.
- *Keep it as documentation only.* Leaves an exported, untested type that reads as load-bearing and re-accretes the drift on the next audit.

**Consequences.**
- `core::RuntimeCommand` and its `pub use` are gone; `DisplayViewId` is no longer imported by `core::command` (only `RuntimeEvent`'s `MatchRuleId` remains). No functional change — nothing referenced the enum (160 lib + 6 profile + 7 integration tests, clippy `-D warnings`, fmt all unchanged-green after removal).
- The live-control work (§165, mid-run recording) is unambiguously specified by this ADR: add `Listener` methods + the `run_channel` command channel — not a `RuntimeCommand` variant. *(Mid-run recording shipped this way: `Listener::set_recording` → `PipelineRequest::SetRecording`, commit `ae7e541`; Display recording followed with `set_display_recording` → `SetDisplayRecording`, and the match-rule `Record { Display | Both }` actions drive the same pipeline paths. Live match-rule toggle / `MarkNow` remain to do.)*
- **Supersedes** the ADR-008 note that the bridge would "align `RuntimeCommand` with §136 and dispatch it." It won't; `UiCommand` is that bridge.
- Spec §136 is amended to document the method-API command surface in place of the enum (version-bumped with a revision note, per the workspace versioning rule).

## ADR-013 — Raw and Display recording are independently configured

**Status:** Accepted. **Context:** spec §53 (Raw recording), §54 (Display recording), §79 (Recording Configuration), ADR-010 (the v2.0 stream-only strip), ADR-012 (live recording via `set_recording`).

**Problem.** `.raw` and `.disp` recording were always **architecturally separate**: Raw taps the verbatim received byte stream (before the old `extract()`), while Display records the *rendered* view output downstream. In `ChannelPipeline::ingest` they are still **separate fan-out taps** to this day. But the v2.0 strip, collapsing `extract()`/decode away, left a single `RecordingConfig` with one `mode: RecordingMode` (Disabled/Raw/Display/Both) and **one shared** `destination`/`file_rotation`/`overwrite`/`timestamps`. So a user could not, e.g., record Raw to one file and Display to another, and the GUI had to cram both behind one mode radio. The merge was only ever in the *config*, never the data path.

**Decision.** Split the config to match the data path. `ChannelConfig.recording: RecordingConfig` becomes two independent fields:
- `raw_recording: RawRecordingConfig` — `enabled` + its own `destination`/`overwrite`/`rotation`/`timestamps` + the `disk_guard` (the guard protects long Raw captures, §168).
- `display_recording: DisplayRecordingConfig` — `enabled` + its own `destination`/`overwrite`/`rotation`/`timestamps`.

`RecordingMode` (Disabled/Raw/Display/Both) is retired — its four states are now two independent `enabled` bools, and "Both" is just both enabled (to two destinations), which the old single-destination config could never express.

**Enabled vs. armed.** `raw_recording.enabled` controls *auto-start at channel Start*. The live Record toggle (ADR-012) is **armed by the presence of a destination**, independent of `enabled` — so a channel can be set up to record-on-demand (destination set, `enabled = false`) and toggled at runtime with no restart.

**GUI placement (the change's user-facing intent).** Raw recording gets its own collapsing panel **above** Configure: the header summarizes live state (●/■ + on/off), and inside are the live Record/Stop toggle plus the Raw setup. Display recording lives **under** Configure as "Display record" (it is display configuration). *(Since revised: "Record Display" now sits directly under the Raw block with the same header controls — state glyph, "Record on start", live Record/Stop — and its setup edits apply live like Raw's, via `set_display_recording`/`SetDisplayRecordingConfig`.)*

**Why not the alternatives.**
- *Keep one shared config.* Cannot express independent Raw/Display destinations, contradicts the separate taps, and forces the awkward single mode radio. The split is what the architecture always implied.
- *Migrate old profiles.* A serde shim mapping the old `[recording]`/`mode` table onto the new fields. Rejected for a **clean break** (bump `schema_version` 2 → 3; old profiles refused with "recreate the profile"), consistent with the v1→v2 precedent (ADR-003) — profiles are dev-only today, so no migration code to carry.

**Consequences.**
- `schema_version` bumped to **3**; v1/v2 profiles refused. Spec §79 rewritten (version-bumped with a revision note, per the versioning rule).
- Runtime `build_raw_recorder`/`build_display_recorder`/`record_arming`/disk-guard read their own config; the pipeline data path is unchanged (taps already separate).
- A channel can now run **both** recordings to two destinations at once — pinned by `raw_and_display_recording_run_to_independent_destinations`. Existing rotation, live-record, and enable-failure tests updated to the split config.

## ADR-014 — Recording destinations must be unique; enforced by unique channel names + an OS advisory lock

**Status:** Accepted. **Context:** spec §55 (recording start), §59 (rotation / filename generation), §71 (configuration validation), §79 (recording config), §121 (file safety), ADR-013 (independent Raw/Display recording).

**Problem.** Two Channels can be configured to record to the **same file**. The §121 `OverwritePolicy` guards against clobbering a *pre-existing* file, but it does **not** stop two *live* Channels from opening and interleaving writes into one destination — each passes its own enable check, then both write, corrupting the capture. This is easy to hit: duplicate the obvious destination across two channels, or (because the channel name appears in rotating filenames, §59) run two same-named rotating channels into one folder. The earlier spec explicitly allowed duplicate names (§6: "Channel Names … need not be unique"), which made the rotating-filename collision reachable by construction.

**Decision.** Two complementary layers: the first removes the most common collision by construction, the second is the race-free enforcement point. (A third layer — an in-process pre-check at Start that named the conflicting channel — was prototyped and **removed**: it faulted the whole *channel*, which wrongly stopped reception and showed the bind/port recourse. A recording-destination collision must be a *recording* fault, leaving the channel Running, so enforcement belongs at the recording-arming point, not channel Start.)

1. **Unique, filesystem-safe Channel Names (necessary, not sufficient).** Channel Names become **unique** in addition to the existing filesystem-safe rule (§59/§71). Enforced at add-channel (a per-kind monotonic, never-reused suffix — `UDP_Channel1`, `UDP_Channel2`, …), at rename (a duplicate is not committed and warns inline), and on profile load (a loaded workspace with duplicate names is rejected per §71's per-channel validation). This makes the **rotating** collision impossible: `<channel>_<period>.raw` leaf names cannot collide if names are unique. It does **not** cover the non-rotating case, where the destination is a full path the user typed — different names can still point at the same file.

2. **OS advisory lock for the recording's lifetime.** When a recording opens its file, the recorder takes a cross-platform **advisory exclusive lock** on a companion `<path>.lock` file and holds it until finalize; failure to acquire surfaces as a recording fault (`RecordingFaulted`) that leaves the **channel Running** (reception continues; only recording is off). This catches both two channels here *and* a **second `listener` process**, and closes the start-race window. Implementation notes:
   - The lock is on a **`<path>.lock` companion**, not the data file, so it is independent of the overwrite policy (Refuse must still fail atomically via the data open; Overwrite/Append must not be clobbered by the lock handle) and is taken *before* the data file is opened, so a lock conflict never touches the destination.
   - The lock holder is a **synchronous `std::fs::File`** (using std's own `File::try_lock`, stable since Rust 1.89; no extra crate). A `std::fs::File` closes *deterministically* on drop, releasing the lock the instant the recorder drops — so a Stop→Start can immediately re-lock. A `tokio::fs::File` is unsuitable: it closes the OS handle asynchronously, so its lock would linger past drop and a restart would spuriously fail.
   - The `<path>.lock` companion is **left on disk** (not deleted on finalize) — the conventional lock-file lifecycle; deleting it would race with another holder. It is empty and reused next session, so stray `.lock` files alongside recordings (one per period with rotation) are expected, not a leak.

**Robustness scope (explicit).** On a **local filesystem** the lock is robust on Windows, macOS, and Linux. Two residual gaps are inherent and identical on every OS, not platform bugs: (a) an *unrelated external program* that writes without locking is unaffected on Unix (advisory locks; Windows is mandatory, so it is actually stronger there); (b) over a **network filesystem** (NFS/SMB) advisory locks are unreliable — there the unique-name layer still prevents the rotating collision, and we accept the residual risk for an explicit shared full path. Both are documented, not silently assumed away.

**Why not the alternatives.**
- *Rely on `OverwritePolicy` alone.* It only guards a pre-existing file; it cannot stop two concurrent live writers (the actual failure here).
- *OS open-mode share semantics instead of an explicit lock.* Not uniform: Windows denies a concurrent writer by default, Unix does not. An explicit advisory lock is the same code path on all three OSs. And `O_EXCL`/`CREATE_NEW` only acts at create time, conflicting with Append.
- *An in-process pre-check at Start that names the conflicting channel.* Prototyped, then removed: it faulted the whole channel (stopping reception, showing the bind/port recourse). A destination collision is a *recording* fault, not a channel fault — the lock enforces it at the recording-arming point and leaves the channel Running. The friendly "used by channel X" message is lost; the recording-fault diagnostic still says the destination is in use.
- *Name-derived filenames only (full structural uniqueness; the deferred #3).* Strongest prevent-by-design, but it removes the ability to pick an exact filename (destination becomes a folder) and needs profile migration — a larger UX decision, deferred to Appendix A.

**Consequences.**
- §6 changes: Channel Names are now **unique** (was "need not be unique"). §71 validation gains the uniqueness rule alongside filesystem-safe. §55/§121 gain the destination-lock requirement; a collision is a recording fault that leaves the channel Running.
- No new crate dependency: the advisory lock uses std's `File::try_lock` (stable since 1.89; MSRV is 1.95). `fs4` stays for the disk-space free functions only.
- The lock feeds the existing `RecordingFaulted` event/diagnostic (ADR-013) — no new GUI surface needed.
- Tests: a name-uniqueness validation test; a lock-conflict recorder test; an orchestrator test asserting the second channel stays Running while its recording faults on the destination lock.

## ADR-015 — Drop the `Highlight` match action; keep listener simple

**Status:** Accepted. **Context:** spec §50.2/§165 (Find & Triggers actions), ADR-010 (which introduced `Highlight` in the v2.0 stream model). Supersedes the `Highlight` portion of ADR-010.

**Problem.** `Highlight { style }` was one of five `MatchAction` variants — it was meant to style a matched **byte range** in the on-screen scrollback. It was never rendered: the GUI treated it as a no-op (`MatchAction::Highlight { .. } => {}`) and the styling was always "TODO." Making it real requires a **byte→display-position map across all three view modes** (Hex/Raw/Rendered), each with variable-width glyph rendering and soft-wrapping — a genuinely fiddly subsystem. Weighed against listener's "keep it simple" goal, the on-screen styling was disproportionate to the need: the concrete "getting ready to record" use case is served by **`Mark`**, which drops a `‹MARK …›` marker into the display recording (`.disp`) with no rendering machinery at all.

**Decision.** Remove the `Highlight` action and its `HighlightStyle` config type. Keep the rest of Find & Triggers unchanged: `BytePattern`/`Idle` conditions and the `Record`/`Mark`/`Notify`/`PauseDisplay` actions. `Mark` remains the correlation primitive (into `.disp`, never `.raw`, §49). The byte-offset-precise on-screen rendering that `Highlight` would have needed is not built.

**Why not keep it inert.** A no-op variant invites revival and keeps a `HighlightStyle` config type + a spec paragraph that describe behavior that doesn't exist. Removing it makes the code honest and the schema smaller; a profile can no longer carry a `kind = "Highlight"` action.

**Consequences.**
- `MatchAction::Highlight` and `HighlightStyle` removed from the config schema (§50.2 / §72 struct listing) and the runtime. `MatchAction` is `#[serde(tag = "kind")]`; a profile with a `Highlight` action now fails to parse (profiles are dev-only; consistent with the ADR-013 clean-break precedent — no migration, no `schema_version` bump for a variant removal).
- Spec §50.2/§165 and the overview drop `Highlight` from the action list; ADR-010's historical text points here.
- Manual/interactive highlighting (a later, larger idea) is **not** pursued; if on-screen styling is ever wanted, it returns as a fresh decision with the position-map cost understood up front.

## ADR-016 — `Mark` carries an inline arrival timestamp, rendered by string-splice (no position map)

**Status:** Accepted. **Context:** spec §50.2/§165 (Find & Triggers), §54/§57 (Display Recording & timestamps), §133 (timestamp model), ADR-013 (independent Raw/Display recording), ADR-015 (why `Highlight`'s on-screen styling was dropped). Builds on the "keep listener simple" line ADR-015 drew.

**Problem.** The concrete "getting ready to record" workflow wants a **timestamp next to a byte pattern** — e.g. the local arrival time immediately before each `$GPGGA` — visible both in the live display and in the Display Recording (`.disp`), while the `.raw` stream stays byte-exact (§53). ADR-015 had just rejected `Highlight` because styling a matched **byte range** needs a byte→**screen-coordinate** map across all three view modes. The open question was whether an inline timestamp inherits that same cost.

**Decision.** Give `Mark` an optional `MarkTimestamp { position: Before | After, format: TimestampConfig }`. On a firing, format the matched chunk's **arrival** `wall_clock` (§133) in **local** time (a listener-local `TimestampConfig` mirroring talker's, but `Local` not `Utc`) and **splice that string into the rendered text at the match's byte offset** — before or after the matched bytes. Render it uniformly in Raw/Rendered/Hex via `DisplayView::render_text_annotated`, which walks the bytes (already ordered) and inserts the annotation string when the walk reaches the target offset. The `.disp` recorder writes exactly what the display shows (the splice happens pre-record), so the live view and `.disp` are identical. `.raw` is never touched. A bare `Mark` (no timestamp) keeps the `‹MARK …›` marker-line behaviour.

**Why this is *not* the `Highlight` cost.** Splicing a string during rendering is a **text insertion**, not a coordinate mapping: no glyph-width measurement, no soft-wrap position math, no per-mode screen geometry. The renderers already produce their output by walking bytes/characters in order; "when you reach offset N, emit this string too" is a one-line addition per mode. The one place a byte offset must meet the character stream (multi-byte UTF-8/UTF-16 in Rendered/Raw) is handled by `decode_with_offsets`, which pairs each decoded character with its source byte offset — a small, tested helper, not a subsystem.

**Also removed (one timestamping mechanism, not three).** Two abandoned pieces are deleted so the per-match `Mark` is the *only* timestamp path: (a) the per-chunk Display-Recording timestamp (`DisplayRecordingConfig.timestamp_enabled` + its GUI checkbox), which was never surfaced meaningfully; and (b) the spec-only `TimestampDisplay`/`TimestampSource`/`TimestampResolution` types, which had no implementation. The byte-exact **Raw Recording timestamp sidecar** (`.raw.idx`, §57) is **kept** — it stays out of the `.raw` bytes and is byte-exact-safe — but remains config-only (`RawRecordingConfig.timestamp_enabled`) with a TODO to add a UI toggle.

**Consequences.**
- `MatchAction::Mark` becomes `Mark { timestamp: Option<MarkTimestamp> }`; new `MarkTimestamp`/`MarkPosition`/`TimestampConfig` config types. `MatchAction` is `#[serde(tag = "kind")]`; a bare `Mark` still parses (the field is `#[serde(default)]`). No `schema_version` bump — additive field plus an additive-safe drop of the display timestamp field (dev-only profiles, ADR-013 precedent).
- The renderer gains `render_text_annotated`/`render_stream_annotated`; `RenderedOutput.timestamp` is retained (it drives time-based Display rotation, §59) but never carries an inline mark — the inline text lives in `RenderedOutput.text`.
- The snapshot's `TriggeredMatch` carries an optional `MarkRender { text, before }` so the live viewer splices the same timestamp the `.disp` got, rebasing the match onto the scrollback window via its view-space `view_offset` (ADR-017 — the stream-space `byte_offset` counts bytes a paused view skipped).
- A minimal in-app editor creates `BytePattern → Mark(+timestamp)` rules; committing uses the existing Apply & Restart path (`config_needs_restart` counts `match_rules`) — no new runtime command. The general match-rule editor (Idle/Record/Notify/PauseDisplay) remains a separate TODO.

## ADR-017 — Two stream offset spaces: stream (all received bytes) vs. view (pause-gated), translated at fire time

**Status:** Accepted. **Context:** spec §50 (Display Pause: "the view stops accumulating new stream bytes"), §50.2 (match firings anchored on byte offsets), §87 (scrollback), ADR-011 (`StreamDelta` offsets), ADR-016 (live-view Mark splicing).

**Problem.** Two offset spaces coexist by construction, and a bug arose from conflating them:

- **Stream space** — `ActivityMeter::total_bytes()`: every byte received since Start. Match firings are anchored here (`FiredRule::match_offset`), diagnostics quote it, and it equals a byte's position in a `.raw` recording that ran from Start.
- **View space** — `stream_dropped + stream_buf.len()`: the scrollback's offsets, used by `StreamDelta`/`stream_end_offset` and the GUI's cursor. Per §50, a paused view stops accumulating, so view space **stops advancing during a pause** while stream space keeps counting; after any pause the two diverge permanently.

The live viewer was rebasing `TriggeredMatch.byte_offset` (stream space) onto its accumulated window (view space), so every post-pause Mark timestamp spliced N bytes late (N = bytes skipped while paused) or vanished. A rule combining `PauseDisplay` + `Mark` hit this immediately. (`.disp` was unaffected — its splice rebases per chunk in stream space consistently.)

**Decision.** Keep both spaces — each is authoritative for its consumer — and **translate at fire time**, where the mapping is exact: in `ChannelPipeline::ingest` the chunk's view-space base (`view_chunk_base`) is known right after the scrollback append (`None` while paused — those bytes have no view position). `apply_fired_rules` stamps each firing with both `byte_offset` (stream space, kept for diagnostics/`.raw` correlation) and a new `view_offset: Option<u64>` (view space); the GUI splices Marks at `view_offset` only. Idle firings carry neither.

**Why not the alternatives.**
- *Unify on one space by making pause advance `stream_dropped` past skipped bytes.* Breaks the ring invariant (`stream_buf[i]` is the byte at offset `dropped + i`) unless the buffer also drops content, which turns pause into a rolling reset and churns the GUI delta protocol.
- *Move pause out of the pipeline into the GUI (never gate the scrollback).* Makes the spaces identical by construction and is attractive long-term, but §50 defines pause as the view not accumulating, resume as "continues from live data, with no backfill" — relocating that behavior is a spec conversation, deferred to the multi-view-pause decision (TODO).
- *Anchor firings in view space only.* Loses the stream-space anchor that diagnostics and `.raw` correlation want, and a firing during pause would have no offset at all.

**Consequences.**
- `TriggeredMatch` gains `view_offset`; a Mark on bytes received while paused is visible in `.disp` (recording never pauses, §58) but not spliced into the live view — the bytes aren't on screen.
- The edge case of a boundary-split match whose carry bytes straddle a pause transition maps approximately (the prior chunk's bytes may not be in the view); `checked_sub` clamps the pre-window edge. Vanishingly rare and self-limiting — the splice is dropped, never misplaced across the window.
- Pinned by `match_view_offset_tracks_the_paused_view_not_the_raw_stream` (pipeline).

## ADR-018 — The `.disp` is the exact rendered stream; no hard wraps, ever (the Notepad model)

**Status:** Accepted. **Context:** spec §54 ("the recorder writes what the display shows, verbatim"), ADR-010 (chunk boundaries are reception details, never structure), §119 (lossy replacement is for *invalid* input), §59 (rotation), the carried TODO item on `.disp` garbling a multi-byte character split across reads.

**Problem.** The Display recorder rendered each received chunk independently and appended `\n` per chunk, so the `.disp`'s line structure tracked **OS read boundaries**: the same byte stream produced different files over serial vs UDP (different chunking — exactly the divergence ADR-009 removed for the live view), a sentence split across two reads gained an artificial line break, a UTF-8/UTF-16 character split across reads became `U+FFFD`, and Hex/tab column state restarted at every read. The `.disp` was the only output in the system that leaked chunk boundaries.

**Decision.** Per active Display recording, a **streaming renderer** (`display::StreamRenderer`) owned by the pipeline's `ViewRecorder` renders chunk by chunk into the *exact rendered stream* — the same text as rendering the whole stream at once, pinned by a chunking-invariance test (byte-at-a-time == one-shot). State per recording: the undecoded multi-byte **carry** (an incomplete tail held for the next chunk; annotations targeting carried bytes defer with it), the Rendered-mode **tab column**, and a Hex **separator-continuation** flag. The carry is flushed lossily at finalize (a truncated stream *is* invalid data at that point). State starts fresh at each recording begin and spans rotation boundaries (each period file stays contiguous; no artifact is reintroduced at rotation).

**No hard wraps are ever written** — the Notepad model, chosen explicitly: the file stores unwrapped content; soft wrap is a viewer toggle. Line breaks come only from the data (Rendered/Raw-Native CR/LF) and from `‹MARK …›` marker lines, which now frame themselves with explicit newlines. Hex is a continuous separator-joined cell stream (its 16-per-line look is likewise a view-time choice). Wrapping at record time was rejected because the display edge is a view-time, mutable property: the width at write time would bake the window-resize history into the file, the recorder (runtime) cannot see the GUI (ADR-008 layering), headless recording has no display edge at all, and any baked width destroys determinism and diffability. The recorder ignores the view config's `wrapping` accordingly.

**Why not the alternatives.**
- *Keep line-per-chunk and document it.* Honest but wrong: adequate only by coincidence at low rates, and it fails precisely under load, when the `.disp` matters most.
- *Delete just the recorder's `\n`.* Breaks Hex (chunks glue as `41 4243`), tab columns, and marker lines — the newline was load-bearing for all three; the fix is state, not deletion.
- *Hard-wrap at a fixed configured width.* Deterministic, but still a lossy view decision baked into content; deferred unless a real consumer needs it.

**Consequences.**
- `DisplayFileRecorder` appends verbatim (no injected newline); `RotatingDisplayRecorder` still rotates per rendered item on arrival time — a rotation boundary can now fall mid-line, which is the correct trade (files stay individually contiguous; the stream is exact across them).
- The pipeline flushes the renderer tail (`finish()`) into the recording at every finalize path (stop, stop-all, channel stop).
- The live viewer and the `.disp` now agree byte-for-byte for Rendered/Raw content (the viewer re-renders the accumulated buffer, which was always boundary-free).
- Pinned by: `stream_renderer_is_chunking_invariant`, `stream_renderer_rejoins_a_split_utf8_character`, `stream_renderer_hex_separates_across_chunks_and_tabs_keep_columns`, `stream_renderer_defers_an_annotation_for_a_carried_byte`, and the pipeline-level `disp_is_the_exact_rendered_stream_across_read_boundaries`.

## ADR-019 — GUI chrome moves to the shared `wiredata-ui` crate

**Status:** Accepted. **Context:** the GUI merge — talker adopts listener's look and feel, and both apps will eventually offer the same light/dark themes. Counterpart of talker ADR-016, which holds the full rationale and the scope rule.

**Decision.** Listener's GUI **chrome** — the bundled font stack (`gui/fonts.rs`), the named color palette (`gui/theme.rs`), the base widget visuals and style tweaks (the body of `apply_style`), and `widgets::format::human_bytes` — moved verbatim into the new internal workspace crate **`wiredata-ui`** (egui-only, `publish = false`). The listener modules remain as thin re-exports, so every call site is unchanged. The palette carries both `LIGHT` and `DARK` instances — several light values (e.g. `WARNING_AMBER`, `INFO_GREY`) are unreadable on a dark backdrop. *(Follow-up, same series:)* listener since gained the theme toggle: `gui/theme.rs` became theme-aware — same names as small functions that read a UI-thread-written mirror of the active egui theme (set at startup restore and by the ◐ header button; persisted under the same `dark_mode` storage key talker uses), picking `LIGHT` or `DARK` live. The pure color helpers (`status_color` etc.) stay argument-free that way. Stream-view content colors remain user-chosen per view (`ColorScheme` offers both light and dark schemes) — deliberately not coupled to the chrome theme.

**Consequences.**
- `listener/assets/fonts/` moved to `wiredata-ui/assets/fonts/` (README and licenses included); the font rationale doc lives there now.
- Anything app-specific stays put: view-models, widgets with runtime knowledge, the stream-view `ColorScheme` (user-chosen content colors are not chrome).
- A change to the shared look lands in both apps by construction — the drift risk that motivated the crate is gone.

## ADR-020 — Terminal transport fault causes use lossless post-reception delivery

**Status:** Accepted 2026-07-12. **Context:** spec §94/§95/§101, ADR-006
(polled lifecycle truth), and ADR-007 (advisory live-stall notices).

**Problem.** A spontaneous transport fault has two observable parts: the lifecycle
state (`Faulted`) and the cause retained in channel diagnostics. The state already
self-corrects through polled snapshots, but the cause was sent with `try_send`
through the same 16-entry queue as advisory `ReceptionStalled` warnings. If that
queue was full at failure time, the only explanatory string was discarded even
though the receive hot path had already ended and no longer needed protection from
blocking.

**Decision.** Keep one bounded transport-condition queue and give its variants two
delivery classes. `ReceptionStalled` remains advisory and uses `try_send` from the
live Serial receive loop. `TransportFaulted` is sent by the runtime monitor only
after joining a failed transport and awaits queue capacity; TCP connection
supervision uses the same helper. `run_channel` remains alive until both ingest and
notice senders close, so it drains the terminal cause before retaining diagnostics.

**Consequences.** Reception throughput and cancellation behavior are unchanged:
only a post-reception monitor may wait. A full advisory queue can delay fault
finalization but cannot erase its reason; a pipeline that has already been forcibly
removed simply closes the receiver and releases the send. Pinned by
`terminal_fault_waits_for_space_in_a_full_notice_queue` plus the end-to-end
`spontaneous_transport_fault_emits_channel_faulted` diagnostic assertion.

## Open questions

_None open. (OQ-L1 resolved by ADR-004 above.)_
