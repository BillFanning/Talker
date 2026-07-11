# TODO — Talker

Implementation reminders for the `talker` application and the workspace as a whole.
Tasks specific to the `nmea0183` library live in
[`nmea0183/docs/TODO.md`](../../nmea0183/docs/TODO.md). Not architectural decisions
(those go in the ADR's Open questions section).

Cross off items as they are completed. Add new ones inline as they come up.

---

## Profiles

- [ ] **Make the profile directory user-configurable.** Default stays the OS config
  dir (`dirs::config_dir()/talker/profiles`, `core::profile::default_dir`) — the safe,
  always-writable choice. Add an override so users can point talker at a directory of
  their choosing (e.g. a `--profile-dir` CLI flag, a `TALKER_PROFILE_DIR` env var, and/or
  a GUI setting), which also enables a portable "profiles next to the .exe" layout
  without making it the default. Sample profiles ship in `talker/profiles/`.

## Docs

- [ ] **Spec §8.1 wording tighten (next spec pass, no bump alone).** The v2.1
  intro says the priority queue is the conceptual model (the implementation is a
  linear next-fire scan), but the first "Queue model" bullet still reads "maintains
  a priority queue sorted by next-fire-time" — tighten to "tracks each message's
  next-fire-time (conceptually a priority queue)". Fold into whichever spec revision
  lands next; not worth a version bump alone. (External review, 2026-07-10.)
- [ ] **Spec §3.2 detail-header readouts (next spec pass, no bump alone).** The
  detail header now mirrors listener's channel block (GUI-merge harmonization,
  2026-07-10): status glyph + name row, `status · interface` row, sent totals +
  throughput (byte- and message-based), and performance readouts (`Status queue`
  occupancy/peak vs `gui::STATUS_QUEUE_CAP`, `Display updates dropped` from
  `TalkerStatus::Sent::dropped_statuses`, `Missed sends` from
  `Schedule::missed_sends`). Also from the same harmonization pass: the
  lifecycle buttons are listener's labeled pair (`start_button` decision), the
  Profile menu moved to the channel-list header (the top-bar name field is
  gone; renaming = Save As…), channel removal is the list rows' ✕ overlay, and
  the detail sections are titled "Configure connection" / "Configure messages"
  (listener's Configure section was renamed to "Configure connection" to
  match). Fold the layout and the two new `Sent` fields (`total_bytes`,
  `missed_sends`) into the next spec revision.

## Robustness & performance (external review, 2026-07-11)

- [x] **Send-failure storms: edge-trigger + backoff** — DONE (`6d4da3b`). A
  send failure opens a `FailureEpisode` (runner.rs): first failure reported
  (`ConnectionError`), further due fires suppressed under the bounded-backoff
  policy (`RETRY_BACKOFF_INITIAL` 250 ms doubling to `RETRY_BACKOFF_MAX` 5 s),
  first success closes it with `SendRecovered { failures, suppressed }`.
  Keeps-running contract retained (no manual Retry state); a successful
  interface update pulls the next retry forward. Pinned by
  `repeated_send_failures_report_one_connection_error` +
  `recovery_reports_send_recovered_with_episode_counts`.
- [ ] **Telemetry split — see proposed ADR-018 (needs acceptance).** Every send
  emits a payload-bearing `TalkerStatus::Sent`; the GUI front-drains a 200-item
  Vec and rebuilds the full output string per repaint (`gui/display.rs`, the
  `widgets.rs` selectable label). Split into periodic counters, sampled display
  batches, and immediate edge-triggered errors; per-send payload delivery only
  for CLI `--echo`.
- [ ] **Observer-path allocations** (behind the workspace benchmark harness
  below): compiled static messages should send without a fresh wire `Vec` per
  send (`render_into`/reusable buffer, `core::message::compile`); cache
  `Schedule::min_active_interval` instead of rescanning each runner loop pass;
  replace the GUI output Vec with an incremental row ring + `show_rows`
  (listener's rendering lessons). Adopt a scheduler heap only if benchmarks
  show message-count scans matter.
- [x] **Command acks** — DONE (`6d4da3b`). Failed `Stop`/interface-update/
  `SetInterval` sends surface in the channel's error banner via
  `command_not_delivered` (gui/mod.rs), distinguishing queue-full (runner
  wedged) from runner-exited (a moot Stop stays silent). Listener's sibling
  landed in the same commit (listener TODO).
- [ ] **TalkerSupervisor — see proposed ADR-019 (needs acceptance).** Channel
  lifecycle, thread collection, draining, counters, and observer policy live in
  `gui/mod.rs` (~1.5 k lines), contradicting the spec's core-owns-the-channel-
  collection boundary; a `core` supervisor should own the runners and expose
  one API to CLI and GUI — also the path to CLI parity.
- [ ] **Palette bypasses.** Several status/warning/log/destructive colors are
  hardcoded in the talker GUI (e.g. `gui/detail.rs` destructive red,
  `gui/mod.rs` log-severity colors) instead of coming from
  `wiredata_ui::palette`. Move the semantic colors into shared palette helpers
  (chrome rule, talker ADR-016 / listener ADR-019) — the next useful GUI
  convergence step.

## Workspace items (external review, 2026-07-11)

- [ ] **Benchmark + soak harness — prerequisite for every "(behind benchmarks)"
  item here and in the listener TODO.** There is currently no benchmark
  harness. Cover: multi-channel talker sends at 100–1,000 Hz, failed-send
  storms, listener 64-byte-chunk ingest throughput, match-rule scaling, full
  scrollback eviction, slow-disk recording.
- [ ] **`wiredata-display` extraction — only together with talker adopting the
  incremental renderer** (row-ring item above): the protocol-neutral
  Raw/Rendered/Hex stream machinery could move to an egui-free shared crate so
  talker reuses listener's incremental rendering. Standalone extraction is
  speculative crate surface — don't do it first.

## macOS target (planned, 2026-07-10)

- [ ] **App Nap opt-out in `core::timing` (ADR-017 counterpart).** macOS timers
  are sub-ms (no `timeBeginPeriod` analog needed), but App Nap throttles the
  timers of hidden/occluded apps — the macOS analog of the Windows 11 timer
  throttling we opt out of. Implement `raise()`/`lower()` for
  `cfg(target_os = "macos")`: hold an `NSProcessInfo`
  `beginActivityWithOptions(NSActivityLatencyCritical |
  NSActivityUserInitiated, reason)` token while the high-resolution guard is
  held, end it on release. Needs `objc2`/`objc2-foundation` as a
  `cfg(target_os = "macos")` dependency. The refcount plumbing
  (`ResolutionCounter`) is platform-neutral and already in place.
- [ ] **Platform pass.** Verify `serialport` enumeration on macOS (ports are
  `/dev/cu.*`; prefer `cu` over `tty` devices), eframe/winit windowing, and
  that all `windows-sys` usage stays `cfg(windows)`-gated. Fonts are bundled,
  so no font work expected.

## When writing the project README

- [ ] Document the system packages required on Linux for `eframe` (`libxcb`, `libxkbcommon`, etc.) per ADR-003 consequences.
- [ ] Document MSRV and the `rustup update stable` requirement per ADR-008.
- [ ] **High-rate timing section (user doc)** — from the 2026-07-10 timing
  discussion; ADR-017 has the design rationale, this is the user-facing telling:
  - Windows wakes sleeps on a 15.625 ms tick; talker auto-requests 1 ms
    resolution while any schedule has an interval < 32 ms (`core::timing`,
    no elevation needed, released when the last fast channel stops, cleaned
    up by the OS even on a kill).
  - What **Missed sends** means: one missed send = one message transmission
    skipped under the stall policy ("fire once, skip the backlog, stay on
    grid" — cadence over count); `should-have-fired = sent + missed`;
    per-channel total across all messages in the schedule.
  - **Status queue / Display updates dropped** vs **Missed sends**: the first
    two affect only what the Output pane shows (drop-and-count, sends never
    delayed); missed sends means the wire cadence itself broke.
  - Practical rate ceilings: ~100 Hz clean out of the box (post-ADR-017);
    ~500 Hz–1 kHz is wake-quantization territory (hybrid spin-wait was
    considered and deliberately not built — one pegged core per fast channel,
    worse when oversubscribed; revisit only on a real ≥1 kHz UDP need).
  - Serial line-rate math: `payload_bytes × 10 / baud` must fit the interval
    (a 40-byte sentence at 115200 baud ≈ 3.5 ms → 1 kHz is physically
    impossible regardless of timers).
  - Minimized windows: talker opts out of Windows 11 timer throttling at
    startup, so minimized long soaks keep cadence; macOS will need the App
    Nap equivalent (see "macOS target" above).

## Future work — out of scope for spec v2.0

- **AIS as a sendable `talker` payload.** The `nmea0183` crate already builds and parses `!AIVDM`/`!AIVDO` and armors the 6-bit payload, but spec v2.0 §5.1 lists exactly five message formats and AIS is not one of them. Exposing AIS in the `talker` message editor — whether as pre-armored raw bytes or as a structured per-message-type editor (Type 1/5/18/24…) — is a feature beyond the current spec. Revisit only with a spec amendment. See the ADR-012 context note and the 2026-05-22 discussion.

- **Manual transmit / inject for troubleshooting.** Captured here because transmit is `talker`'s job, not `listener`'s (listener is strictly receive-side). Beyond talker's scheduled/profile-driven sends, a troubleshooting workflow wants **ad-hoc, one-shot injection** — type or pick a payload and fire it once at a serial port or network endpoint to provoke a device, while `listener` observes the response on the same or another channel. This is the natural talker counterpart to the listener troubleshooting use case (see the listener "primary use cases" notes, 2026-06). A real feature here needs a spec amendment: define how one-shot/manual sends relate to the §5.1 message formats and the scheduler, and whether it's CLI, GUI, or both. Revisit only with that amendment.

---

## Completed during the spec v2.0 upgrade

The sections below were open in TODO v1.0 and are now done; kept here so the history is not lost.

- **`core::logging`** — `tracing-appender` added to workspace dependencies for rotating file output; dual-mode subscriber implemented (CLI writes to stdout/file; the GUI captures events into a `tracing_subscriber::Layer` that forwards to the UI thread via `crossbeam-channel`).
- **`core::profile`** — schema v2 with `CURRENT_VERSION` checked on every load; `#[serde(default)]` on all fields; `#[non_exhaustive]` on profile enums. OQ-2 (`toml = "1"` is sufficient) and OQ-3 (profiles use a `talker`-side NMEA representation, so the `nmea0183` `serde` feature stays off) are resolved — see the Open questions section of the ADR.
