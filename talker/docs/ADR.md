# Architecture Decision Record — Talker
**Project:** talker  
**Version:** 1.6
**Date:** 2026-07-27
**Status:** Accepted

Revision note (truthful pushed-snapshot freshness):

- **ADR-042** records the runner-supplied capture instant and explicit final
  provenance that distinguish current, expired, and final timing evidence without
  adding a dormant telemetry heartbeat. Capacity, Cadence, and detailed Timing now
  consume one freshness classification, and expired recent timing cannot silently
  drive measured headroom.

---

This file records workspace-level and `talker`-application decisions. Decisions
specific to the `nmea0183` library crate live in
[`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) (currently ADR-009 and OQ-4).
ADR numbers are shared across both files and never reused, so the cross-references
below (e.g. "see ADR-009") remain valid.

---

## What belongs in an ADR

An ADR captures *why* a significant decision was made, not just *what* was decided. It records the context, the options considered, the choice made, and the consequences — so that anyone joining the project later (or the original author six months later) can understand the reasoning without reconstructing it from scratch. An ADR is not a specification; it complements the spec by explaining the decisions that shaped it.

---

## ADR-001 — Workspace structure: split reusable NMEA support from `talker`

**Context:** The NMEA 0183 module was identified early as reusable across other projects. The question was whether to keep it as a module inside the `talker` binary or make it a separate library crate.

**Decision:** The project is structured as a Cargo workspace, initially with `talker` (binary) and `nmea0183` (library). The `nmea0183` crate has no dependency on `talker` and no knowledge of its internals. The workspace now also contains `listener`, a sibling receive crate; it dropped NMEA decoding in its v2.0 stream-only pivot (listener ADR-010). Listener v2.2 reuses `nmea0183` only to construct presentation-only ZDA Mark annotations (listener ADR-025); received bytes remain undecoded.

**Alternatives considered:**
- Single crate with `nmea0183` as an internal module: simpler initially, but makes future extraction painful — splitting a module into a crate after it has grown requires touching import paths throughout the codebase.
- Separate repository: too much overhead for a project at this stage.

**Consequences:**
- `nmea0183` can be published to crates.io independently when ready.
- `nmea0183` must not depend on application-level crates (`anyhow`, `eframe`, etc.).
- Workspace `Cargo.toml` manages shared dependency versions; member crates reference them with `{ workspace = true }`.

---

## ADR-002 — Async runtime: none (`std::thread` + `crossbeam`)

**Context:** `talker` needs to run a UI thread, one talker thread per active connection, and a logger thread concurrently without any blocking the others. Multi-port simultaneous output is a first-class design goal, not a future option.

**Clarification:** "No async runtime" and "multiple OS threads" are entirely independent concepts. This decision rejects cooperative async scheduling (Tokio tasks); it does not restrict the use of OS threads. `talker` uses multiple OS threads throughout.

**Decision:** Use OS threads (`std::thread`) with `crossbeam-channel` for all inter-thread communication. No async runtime (Tokio, async-std, etc.) is used. Each active connection runs in its own dedicated talker thread.

**Alternatives considered:**
- **Tokio:** The dominant async runtime in Rust. Excellent for high-concurrency network servers. Rejected because: (a) `serialport` is a synchronous, blocking API and integrates poorly with async — calls must be wrapped in `spawn_blocking`, which adds overhead and complexity without benefit; (b) `eframe`/`egui` is synchronous; bridging it to an async executor adds friction; (c) `talker` manages a bounded number of connections — the scalability benefits of async do not apply.
- **Multiple application instances:** Running one `talker` process per output port was considered for simplicity but rejected. Each `eframe` instance carries a full GPU-backed rendering stack; this approach is resource-wasteful and unworkable at any meaningful scale.
- **Rayon:** Work-stealing thread pool, designed for data parallelism. Not appropriate for this use case.

**Consequences:**
- Each talker thread is a plain OS thread, easy to reason about and debug.
- Each talker thread waits without spinning by blocking on its command channel with a deadline: `Receiver::recv_deadline(next_fire)` when a message is scheduled (wakes exactly at the fire time, or immediately for a command), and `recv()` when the schedule is idle. A disconnected command channel ends the loop, so a dropped handle cannot leak the thread. (An earlier draft of this ADR specified the `crossbeam-channel` `select!` macro; the single-command-channel deadline receive is simpler and equivalent — there is only one channel to wait on, since the schedule timer is a computed deadline, not a channel.) The loop lives in `core::runner`, shared by the CLI and GUI (spec §2.2).
- Each active connection has its own dedicated channel pair with the UI thread.
- `core::channel` manages a collection of channel instances from the initial implementation; there is no single-channel shortcut to be refactored later.
- The number of simultaneous connections is bounded by available system resources (serial ports, network sockets), not by any artificial limit in the software.

---

## ADR-003 — GUI framework: egui / eframe

**Context:** `talker` needs a cross-platform GUI (Windows, macOS, Linux) that is utilitarian, data-dense, and maintainable by a small team.

**Decision:** Use `egui` (immediate-mode GUI library) via `eframe` (the official native/web framework wrapper).

**Alternatives considered:**
- **iced:** Elm-architecture (message-passing) GUI. More idiomatic for Rust's ownership model in some ways, but less mature ecosystem and steeper learning curve for a data-heavy control panel UI.
- **Tauri:** Web-based UI layer over a Rust backend. Excellent native look, but introduces a JavaScript/HTML/CSS front-end layer and a more complex build process — unjustified for a utilitarian engineering tool.
- **Native platform bindings (gtk-rs, winapi):** Platform-specific; would require separate implementations per OS.

**Consequences:**
- `egui` does not produce a native-looking UI. This is acceptable for a utilitarian engineering tool where information density and simplicity matter more than visual integration.
- `eframe` has significant compile-time dependencies (wgpu, winit, image crates). Compile times will be longer than a CLI-only binary.
- The `persistence` feature of `eframe` is used for GUI state save/restore.
- Linux users need system packages installed for the graphics stack (`libxcb`, `libxkbcommon`, etc.). This should be documented in the README.

---

## ADR-004 — Error handling strategy: `thiserror` in library, `anyhow` in application

**Context:** Rust requires explicit error handling. Two popular approaches exist for reducing boilerplate.

**Decision:** 
- `nmea0183` uses `thiserror` to define a typed public error enum (`NmeaError`). Callers can match on specific variants.
- `talker` uses `anyhow` for application-level error propagation. Errors are wrapped with `.context()` to produce rich diagnostic messages for logging and display.

**Alternatives considered:**
- `anyhow` everywhere: Loses the ability for callers of `nmea0183` to programmatically distinguish error types (e.g., `ChecksumMismatch` vs `InvalidField`). Not appropriate for a reusable library.
- `thiserror` everywhere: More boilerplate in application code where callers don't need to distinguish error types. Not worth the cost in `talker`'s own modules.
- `Box<dyn Error>`: Lowest common denominator. No structured context, no ergonomic `?` chaining with wrapping. Rejected.

**Consequences:**
- The boundary between `nmea0183` and `talker` is where `NmeaError` gets wrapped into `anyhow::Error` via `?`.
- New error variants in `nmea0183` are a minor breaking change for `nmea0183`'s public API — adding `#[non_exhaustive]` to the error enum is recommended before any external publication.

---

## ADR-005 — Profile format: TOML

**Context:** User profiles (connection params, schedule, data config) must be saved, loaded, edited, and potentially version-controlled outside the program.

**Decision:** Profiles are serialized to TOML using `serde` + the `toml` crate.

**Alternatives considered:**
- **JSON:** Machine-readable but noisy for human editing (mandatory quotes on keys, no comments).
- **YAML:** Human-friendly but has well-known parsing footguns (the Norway problem, implicit type coercion).
- **INI/custom format:** Would require writing a custom parser. No benefit over TOML.
- **Binary (bincode, messagepack):** Not human-readable. Violates the explicit requirement.

**Consequences:**
- TOML files can be commented, diffed, and committed to version control.
- Profile structs must derive `serde::Serialize` and `serde::Deserialize`.
- Adding new fields to a profile struct requires a migration strategy (use `#[serde(default)]` for backwards compatibility).

---

## ADR-006 — Logging: `tracing` + `tracing-subscriber`

**Context:** `talker` needs structured logging to both a rotating file and stdout (CLI), and to a GUI status pane plus optional file (GUI).

**Decision:** Use the `tracing` facade with `tracing-subscriber` for log dispatch. A custom subscriber layer will route `ERROR`/`WARN`/`INFO` events to the appropriate sinks depending on interface mode.

**Alternatives considered:**
- **`log` + `env_logger`:** The classic Rust logging pair. Simpler but less flexible — `tracing` supports structured fields and spans, which will be useful for correlating log events with specific connections or send operations.
- **`slog`:** Structured logging with explicit loggers passed through the call stack. More explicit but significantly more verbose.

**Consequences:**
- The GUI status pane is implemented as a `tracing_subscriber::Layer` that captures log events and pushes them to the UI thread via a `crossbeam-channel`.
- Log level filtering is controlled by the `RUST_LOG` environment variable in CLI mode, and by a settings toggle in GUI mode.

---

## ADR-007 — Checksum/CRC: `crc` crate

**Context:** `talker` must compute XOR, CRC-8, CRC-16/CCITT, CRC-16/MODBUS, and CRC-32 checksums for outgoing data. The `nmea0183` crate also computes NMEA XOR checksums.

**Decision:** Use the `crc` crate for all CRC computations. Implement the NMEA XOR checksum directly in `nmea0183` (it is a trivial one-line fold, has no external dependency, and keeps the library self-contained).

**Alternatives considered:**
- `crc32fast`: Only CRC-32. Too narrow.
- `crc16`: Unmaintained.
- Rolling our own: Unnecessary given the quality of the `crc` crate.

**Consequences:**
- `nmea0183` has no dependency on the `crc` crate — its checksum is a byte XOR, implemented inline.
- `talker`'s `core::message` module uses `crc` for the general checksum feature.
- The `crc` crate uses a const-generic algorithm table approach; algorithm selection is a compile-time or runtime parameter depending on usage pattern.

---

## ADR-008 — Minimum Supported Rust Version (MSRV): 1.95

**Context:** `talker` depends on several crates whose own MSRV has crept upward over time (`clap` 4.6 requires 1.85, `crc` 3.4 requires 1.83, `eframe`/`egui` 0.34 targets recent stable). Pinning to an older Rust version forces the workspace to also pin older versions of these crates, which compounds with every release. The team has no constraint requiring older toolchains.

**Decision:** MSRV is set to Rust 1.95 (current stable as of April 2026) in `[workspace.package]`. Crates in the workspace track the latest stable Rust release rather than supporting an extended back-compatibility window.

**Alternatives considered:**
- **MSRV 1.75 (the original choice):** Was a reasonable "recent baseline" in 2025 but is now ~2.5 years old. Keeping it forced version pins on `clap`, `crc`, and likely `eframe`/`egui` — a moving maintenance burden that produced no benefit, because no team member or known user requires an older toolchain.
- **MSRV at the oldest version that builds with current crate versions:** Saves nothing in practice; the workspace tracks current stable either way, and a precise "minimum" figure is overhead to maintain.
- **N-2 or N-6 month policy:** Appropriate for widely-published libraries serving cautious downstream users. Unnecessary for a workspace whose sole consumer is its own developers.

**Consequences:**
- Users must run `rustup update stable` before building. This is documented in the README.
- Every dependency can be specified by major version only (`clap = "4"`, `crc = "3"`, etc.) and resolved to the latest compatible release.
- CI tests against stable. The declared MSRV is bumped to match current stable on each Rust release rather than maintained as a separate floor.
- This MSRV applies to the workspace and the `talker` binary. The MSRV policy for the `nmea0183` library — which is intended for crates.io publication and may want a looser, more downstream-friendly MSRV — is deferred to a future ADR when publication approaches.

---

## ADR-009 — Talker ID and sentence type extensibility in `nmea0183`

Moved to [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) — it is a decision internal to the `nmea0183` library. The ADR-009 number is retained there.

---

## ADR-010 — Profile and GUI state separation

**Context:** Profiles need to be compatible between CLI and GUI. GUI also needs to save window geometry and layout. The question was whether these should share a format and file, or be kept separate.

**Decision:** Profile data and GUI state are strictly separated into two different files with two different purposes:

- **Profiles** — TOML files containing connection configuration, data configuration, schedule, and checksum settings. Fully compatible between CLI and GUI. Stored in a documented profile directory. Schema is public and documented so users can create and edit profiles by hand.
- **GUI state** — `eframe`'s built-in persistence mechanism (ron format, platform config directory). Contains window geometry, panel layout, display column toggles, and the name of the last active profile. Never loaded by the CLI. Never contains connection or data configuration.

**Alternatives considered:**
- Single file for everything: Simpler on the surface, but means the CLI must parse and ignore GUI-only fields, and GUI-only concepts leak into the profile schema. Rejected.
- TOML for GUI state as well: Would require reimplementing what `eframe` already provides for free. Not justified.

**Consequences:**
- Profile structs must not contain any GUI-only fields. GUI preferences that relate to a connection (e.g., which display columns are visible for that connection) are GUI state, not profile data.
- CLI loading a GUI-created profile silently ignores unrecognized fields via `#[serde(deny_unknown_fields = false)]` (the `toml` crate default). This ensures forward compatibility as GUI-adjacent fields are never written into profiles in the first place.

---

## ADR-011 — CLI multi-channel model

**Context:** The GUI supports multiple simultaneous connections as a first-class feature. The question was whether CLI mode should be one connection per process or support multiple connections in one process.

**Decision:** CLI mode supports multiple simultaneous channels in a single process, using the same `core::channel` collection and per-channel talker thread model as the GUI. The primary mechanism is `--profile`, which may define one or many channels. Ad-hoc multi-channel via repeated CLI flags is deferred.

**Alternatives considered:**
- One channel per CLI instance: Simple to implement, but requires users to manage multiple terminal sessions and processes for multi-port work. Inconsistent with the GUI model and defeats the profile system. Rejected.
- Repeated channel flags for ad-hoc multi-channel operation: Desirable long-term but adds CLI parsing complexity. Deferred to a future iteration; `--profile` covers the primary use case.

**Consequences:**
- `talker --profile <name>` is the canonical way to launch multi-channel sessions from the CLI.
- The CLI and GUI share identical `core` behavior for channel management. There is no CLI-specific channel limit or shortcut.
- stdout echo in multi-channel CLI mode outputs data from all channels interleaved. Each line is prefixed with a channel identifier to allow filtering.

---

## ADR-012 — Binary field types

**Status:** Superseded by the spec v2.0 message-format model (see ADR-015 context and the message-format summary in [`AGENTS.md`](../../AGENTS.md)). Spec v2.0 removed the typed-binary-field concept: arbitrary byte sequences are now entered in **Hex** format, and structured data is built through the UTF-8/UTF-16/ASCII/NMEA formats. `core::data` and the `BinaryField` enum were never carried into the v2.0 codebase. This ADR is retained for historical context.

**Context:** The spec originally deferred the exact set of binary field types. At the time, a concrete decision was needed before the planned data-construction module could be implemented.

**Decision:** Binary data is constructed as an ordered sequence of typed fields. Supported types are: `u8`, `u16`, `u24`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`, and raw bytes (arbitrary hex). Byte order is selectable per field — big-endian (default) or little-endian.

**Rationale for `u24`:** Three-byte unsigned integers appear frequently in sonar, audio, and oceanographic equipment. Without `u24`, users must construct them manually from raw bytes, which is error-prone. The implementation cost is low.

**Rationale for big-endian default:** The majority of marine and survey instruments use big-endian (network byte order). Defaulting to big-endian reduces misconfiguration for the primary target audience.

**Consequences:**
- The planned data-construction module would have implemented a `BinaryField` enum with one variant per type plus `RawBytes(Vec<u8>)`.
- Each field carries a `ByteOrder` enum (`BigEndian` | `LittleEndian`).
- `u24` requires manual encoding (write the 3 most-significant bytes of a `u32`); no standard Rust primitive maps directly to it.
- Binary message definitions are saved in profiles as an ordered list of field descriptors.

---

## ADR-013 — Profile schema versioning and migration strategy

**Context:** Profile structs will gain new fields as `talker` evolves. Old profile files must load cleanly in newer versions of the program, and users must be warned rather than silently harmed when loading a profile from a newer version.

**Decision:** A two-layer strategy:

**Layer 1 — `#[serde(default)]` on all profile fields.** Every field has a sensible default. Old profiles missing newly added optional fields load without error. This handles the common case with zero migration code.

**Layer 2 — `version: u32` in the profile header, starting at `1`.** Load behavior:
- Version matches current: load normally.
- Version is older: run a versioned migration function, fill in defaults, log a warning, optionally rewrite at the new version.
- Version is newer than the binary understands: refuse to load, warn the user.

The version number increments only on breaking schema changes that `serde(default)` cannot handle alone, keeping migration functions minimal.

**Consequences:**
- All profile structs annotated with `#[serde(default)]` from the first commit.
- A `PROFILE_SCHEMA_VERSION: u32` constant is defined in `core::profile` and checked on every load.
- Migration functions live in `core::profile::migration` as a match on `(from_version, current_version)`.
- The `#[non_exhaustive]` attribute is used on profile enums to prevent external code from exhaustively matching on them, enabling future variant addition without breaking changes.

**Update (v2.0, 2026-05-22):** The spec v2.0 upgrade restructured the profile schema (nested channels, each owning an interface and a list of messages). Rather than write a v1→v2 migration, the project chose a **clean break**: `CURRENT_VERSION` is `2`, and `Profile::load` refuses any profile whose version differs from it — newer versions are rejected as unsupported (Layer 2 as designed), and **older versions (v1) are also rejected**, with an error instructing the user to recreate the profile. The reasoning: v1 had no released users, so migration code would have been dead weight maintained forever. Layer 1 (`#[serde(default)]` on every field) still stands and handles all *additive* schema changes within v2. The `core::profile::migration` module was therefore never created; when a breaking v3 change arrives, a migration step and version-downgrade handling are reinstated at that point.

---

## ADR-014 — `talker` as a library plus a thin binary

**Context:** The `talker` crate began as a pure binary (`main.rs` and a module tree). All application logic lives in `core`; `cli` and `gui` are thin interface layers. During the spec v2.0 upgrade, removing the workspace-wide `#![allow(dead_code)]` exposed nine false-positive dead-code errors: constructors and helpers in `core` that are exercised by unit tests but not yet called from `main`. In a binary crate, anything not reachable from `fn main` is "dead" — even when it is part of a module's intended public API and is under test.

**Decision:** `talker` is both a library and a binary. `src/lib.rs` declares `pub mod cli; pub mod core; pub mod gui;`. `src/main.rs` is a thin shim that calls into the library. The library's public items are part of an exported API, so the compiler no longer flags tested-but-not-yet-wired `core` code as dead.

**Alternatives considered:**
- **Keep `#![allow(dead_code)]`:** Silences the false positives but also silences *genuine* dead code for the life of the project. Rejected — the lint is worth keeping honest.
- **Per-item `#[allow(dead_code)]`:** Scatters annotations across `core` and requires adding/removing them as `main` wiring catches up. Noisy and easy to leave stale.
- **`#[cfg(test)]`-only constructors:** Would mean test-only code paths diverge from production ones. Rejected — tests should exercise the real API.

**Consequences:**
- `cargo test` can address `core` modules directly through the library crate (`use talker::core::...`), and integration tests in `talker/tests/` link against the library.
- The crate compiles with the default dead-code lint active; genuinely unused code is caught.
- `main.rs` contains no logic beyond argument dispatch — consistent with the existing rule that `cli`/`gui` are thin layers.

---

## ADR-015 — Code pages: hand-written tables, no `encoding_rs`

**Context:** Spec §5.2 requires four single-byte code pages for ASCII-format messages — CP437, Windows-1252, Mac OS Roman, and ISO-8859-1 — available on every host OS regardless of the platform's own locale. Each is a fixed mapping between byte values 128–255 and Unicode scalar values.

**Decision:** Each code page is implemented as a hand-written static table in `core::message::codepage`, generated from the authoritative Unicode Consortium mapping files. No transcoding crate is taken as a dependency.

**Alternatives considered:**
- **`encoding_rs`:** The standard Rust transcoding crate (the encoding engine from Firefox). It is correct and well-maintained, but it is a heavy dependency — it carries the full WHATWG Encoding Standard: every legacy multi-byte CJK encoding, big lookup tables, and a streaming decoder API. `talker` needs four *single-byte* maps and a one-shot encode. Pulling in the whole crate for that is disproportionate: it inflates compile time and binary size and widens the dependency surface for no functional gain.
- **`codepage`/`oem_cp` and similar smaller crates:** Lighter than `encoding_rs`, but still an external dependency and an API to track, for tables that are trivially expressed inline and never change.
- **OS-provided conversion APIs:** Platform-specific, and would violate the spec requirement that all four code pages work identically on every host OS.

**Consequences:**
- `core::message::codepage` owns four `[char; 128]`-style tables; encoding is a direct lookup, decoding (for the display pane) is a reverse search.
- The tables were transcribed from the Unicode Consortium `.TXT` mapping files, not from memory; unit tests pin representative code points so a transcription error cannot pass silently.
- ISO-8859-1's 128–255 range is the identity map onto U+0080–U+00FF, so it needs no table — it is handled as a special case.
- Adding a fifth code page later is a self-contained table addition with no dependency change.

---

## ADR-016 — `wiredata-ui`: a shared GUI-chrome crate (fonts, palette, base style)

**Context:** The GUI merge makes `talker` adopt `listener`'s look and feel (channel list + detail layout, listener's light theme, a dark theme for both). Before this decision each app carried its own copies of the same chrome: overlapping bundled Noto font files, two divergent visual styles, and duplicated formatting helpers. A deliberate "the two apps look identical" goal turns that duplication into guaranteed drift.

**Decision:** A fourth workspace crate, **`wiredata-ui`** (internal, `publish = false`), owns the GUI **chrome** only: the bundled font stack and its fallback chains (`fonts`), the named severity/status color palette with `LIGHT` and `DARK` const instances (`palette`), the base widget visuals for both themes plus the shared non-visual style tweaks (`style`), and small pure formatting helpers (`format::human_bytes`). It depends **only on `egui`** — never on `talker`, `listener`, `eframe`, or any runtime crate. App-specific widgets, layouts, and view-models stay in each app. Listener's counterpart decision is its ADR-019.

**Scope rule (what belongs here):** a piece moves into `wiredata-ui` only when it is (a) purely presentational, (b) meaningfully identical across both apps, and (c) egui-only. Anything entangled with an app's runtime, config schema, or view-model stays out. A shared *runtime* crate (`wiredata-core`) was considered during the talker review and rejected — config/timestamp/profile duplication is small and stable, and the apps' I/O shapes are opposite; the chrome is the one place where duplication would actively grow.

**Consequences:**
- The font assets moved from `listener/assets/fonts/` to `wiredata-ui/assets/fonts/` (one copy; talker's private subset — including the Cascadia control-pictures subset font — was deleted, since the full Cascadia face in the shared stack covers U+2400–U+243F).
- Talker's local visuals/font installers were replaced by `wiredata_ui::{fonts::install_fonts, style::install_visuals, style::apply_style_tweaks}`. Talker's light theme is now listener's grey-backdrop look; talker's original dark values seeded the shared dark theme. Both apps now expose and persist the same dark/light toggle.
- Talker also adopts listener's window-startup lessons: `persist_window: false` (no post-show geometry jump) and a minimum window size.
- Listener's `gui/{fonts,theme}.rs` and `widgets/format.rs` became thin re-exports, so its call sites are unchanged.
- The crate has no `docs/` folder; its decisions live in the two app ADR series (this entry and listener ADR-019).

**Follow-up (selected-channel continuity).** The shared crate also owns the purely
presentational selected-channel card, compact tab, row text and historical-count
emphasis, and the card-to-page connector. Each app supplies only a
selected card rectangle, its actual scroll viewport, and (for an expanded list) the
panel id. One shared painter owns the complete page-edge stroke and mirrors egui's
resize hover/drag stroke; it draws the tab detour only when the card and both turns
fit inside the viewport, otherwise it draws an ordinary straight divider. This
prevents partial off-screen connectors and keeps both apps' selection hierarchy and
resizer feedback identical. Lifecycle glyphs and live faults remain app-owned because
they carry runtime meaning rather than chrome.

**Follow-up (2026-07-16, chrome dedup).** The remaining duplicated accessors merged
into the crate: `palette::active(ui)` is the one theme-aware palette accessor
(replacing talker's `theme_palette`, `selection`'s private copy, and listener's
`gui/theme.rs` process-global mirror — listener's pure helpers now take `&Palette`
like talker's `lifecycle_indicator`), `style::theme_toggle_button` owns the shared
◐/◑ header toggle, `selection::{severity_counts_line, last_error_line}` own the two
value-only channel-row lines, and `install_chrome(ctx)` bundles the three-call
chrome install. Listener's re-export shims (`gui/fonts.rs`, `widgets/format.rs`,
`gui/theme.rs`) were deleted in favor of direct `wiredata_ui` paths — the shims
predated the merge settling and had become pure indirection.

---

## ADR-017 — High-resolution OS timer scope for high-rate schedules

**Context:** The runner waits for its next fire with a deadline-bounded blocking receive (`recv_deadline`, ADR-002). On Windows a parked thread with a timeout wakes on the system scheduler tick — **15.625 ms** by default — so a 10 ms interval (100 Hz) can never be honored: every wake lands more than one interval late, the stall policy (spec §8.1) skips the backlog to stay on grid, and ~36% of sends are skipped with ~15.6 ms observed spacing. 10 Hz absorbs the same jitter invisibly, which is why the problem only appears at high rates.

**Options:** (a) request 1 ms resolution process-wide for the whole app lifetime — simple but pays an idle power cost and is against Microsoft guidance; (b) hybrid wait (sleep short, spin the last ~2 ms) — precise to sub-ms but burns a core per fast channel; (c) request 1 ms resolution **only while a schedule actually needs it**.

**Decision:** (c). Talker's policy remains in `core::timing`; ADR-038 later moves
the cross-application OS mechanism into `wiredata-timing`. A refcounted RAII guard
(`high_resolution()` / `HighResolutionGuard`) wraps
`timeBeginPeriod(1)`/`timeEndPeriod(1)`: the first holder raises the request, the last
drop releases it, and the count and OS call share one lock so concurrent
acquire/release cannot reorder the pair. Each runner re-evaluates
`Schedule::min_active_interval() < HIGH_RATE_THRESHOLD` (32 ms = two default ticks)
every loop pass, so `SetInterval` acquires/releases mid-run. Both entry points (GUI
funnel, CLI run) additionally opt out of Windows 11's minimized-window timer
throttling via `SetProcessInformation(ProcessPowerThrottling,
IGNORE_TIMER_RESOLUTION)` — otherwise a minimized long soak silently falls back to
15.6 ms wakes. Everything is a no-op off Windows.

**Consequences:**
- 100 Hz sends hit cadence (wake jitter ~1–2 ms ≪ 10 ms interval); the practical ceiling moves to roughly 500 Hz–1 kHz, beyond which option (b) would be needed.
- No elevation required; per-process since Windows 10 2004, so other processes are unaffected. The kernel releases the request on process death (any kind), so a leaked guard cannot outlive talker — the RAII release is about dropping the power cost early, not correctness.
- The GUI's "Missed sends" readout is the acceptance signal: it should stay 0 at 100 Hz on an otherwise healthy interface.
- ADR-034 later extends the request to bounded final-deadline windows for an
  explicitly Precise slow schedule. Standard retains the automatic high-rate policy
  decided here.

---

## ADR-018 — Talker telemetry split: counters, sampled payloads, edge-triggered errors

**Status:** Accepted 2026-07-11 (proposed by the 2026-07-11 external review).

**Context:** Every send emitted a payload-bearing `TalkerStatus::Sent` over the
per-channel status channel; the GUI maintains a front-drained 200-item display Vec and
rebuilds the complete output string per repaint. Correctness was protected (bounded
queue, drop-and-count, `dropped_statuses` self-correction — sends are never delayed),
but the per-send CPU/allocation cost scales with rate and becomes load-bearing in the
500 Hz–1 kHz territory ADR-017 opened: at 1 kHz the runner allocated and shipped a
thousand payload Vecs a second that the GUI mostly discarded.

**Decision:** The status protocol has three lanes with different cadences, and the
owner picks the payload policy via a named `ObserverPolicy` passed to the runner:

1. **Periodic counters** — `TalkerStatus::Counters` (total/per-message counts, bytes,
   missed sends, dropped statuses; **no payload**), emitted at most once per
   `counter_interval` (default 200 ms ≈ 5 Hz), checked on the send path. A final
   `Counters` is emitted when the runner stops, so totals are exact at rest.
2. **Sampled display payloads** — `TalkerStatus::SendSample` (payload-bearing),
   newest-per-interval: the first send after `sample_interval` elapses carries its
   payload (default 100 ms ≈ 10 Hz). The Output pane shows a live, bounded sample
   instead of every wire message.
3. **Immediate errors** — `ConnectionError` / `SendRecovered` / `OpenFailed`
   (edge-triggered per ADR-017's sibling backoff work) are never rate-limited.

`Sent` is **removed**, replaced by lanes 1+2. CLI `--echo` passes
`ObserverPolicy::every_send()` (every send emits a `SendSample`) — the one consumer
that genuinely wants every payload keeps it, explicitly.

**Consequences:**
- Per-send cost at any rate is a counter bump; allocations for observers happen at
  the sample cadence, not the send cadence. The 200-item display Vec fills at ≤10
  items/s regardless of send rate.
- The self-correcting counter scheme survives: totals ride in every `Counters`, so a
  dropped status is corrected by the next one; drop-and-count is unchanged.
- Status-queue pressure drops by construction (≤ ~15 statuses/s/channel steady-state
  vs. one per send), making `dropped_statuses` a true anomaly signal.
- The GUI's per-send "Output" completeness is gone by design at high rates — the pane
  is a sample, labeled as such; the wire remains exact (that's what recording and the
  CLI `--echo` are for).
- The scheduler/spec "status" wording needs a spec-pass update (see TODO, no bump
  alone).

---

## ADR-019 — Core `TalkerSupervisor` owning the channel collection

**Status:** Accepted 2026-07-11 (proposed by the 2026-07-11 external review).
Implementation deliberately **sequenced after ADR-018** — the telemetry lanes change
the same status plumbing the supervisor will own, and moving them once is cheaper.
The open points below are settled at implementation time, not re-litigated.

**Context:** The spec places channel collection and management in `core`
(`core::channel` manages a collection from day one), but in practice channel
lifecycle, runner-thread collection, draining, cumulative counters, and observer
policy live in `gui/mod.rs` (~1.5 k lines) — the GUI is not the thin layer §127/AGENTS
call for, and the CLI cannot reach that logic (part of why CLI parity lags).

**Decision (implemented 2026-07-11):** `core::supervisor::TalkerSupervisor` owns
index-stable channel slots — runner threads, command/status channel pairs, draining
buckets, and per-channel `ChannelTelemetry` (counts, queue occupancy, errors, the
error banner). The open points settled as:

- **API shape: blocking methods, no command enum.** `start`/`stop`/
  `update_interface`/`set_interval` are direct calls returning a `CommandOutcome`
  (`Enqueued`/`QueueFull`/`NotRunning`); commands that cannot be enqueued are recorded
  in telemetry. Successful enqueue is not proof of execution; ADR-021 adds correlated
  execution results for live mutations. No supervisor thread, matching
  ADR-002 — everything runs on the caller's thread and never blocks (`join_all` is
  the one deliberate exception, exit-path only).
- **Drain cadence: caller-owned poll.** `poll()` non-blockingly folds every runner's
  status lanes (ADR-018) into the telemetry and returns the payload samples; the GUI
  calls it each frame, woken by the notify callback it installs via `set_notify`.
- **Sequencing:** landed after ADR-018 as planned; the lanes moved once.
- **Tests:** the extracted lifecycle is unit-tested in core against mock interfaces
  (start/poll/stop reap + exact totals at rest, command-failure surfacing, restart
  reset + predecessor joins, slot removal), which the GUI-embedded version never was.
- **CLI adoption is deferred to the ad-hoc CLI work.** The current CLI is a one-shot
  headless run whose blocking status funnel is the right shape for `--echo`
  (lowest-latency payload delivery); wrapping it in a poll loop would only add
  latency. The supervisor is the base for the *future* interactive CLI lifecycle
  (the parity item in TODO.md); today both layers consume the same runner API.

**Consequences:**
- The GUI holds pure view-state (drafts, displays, rates, selection, log tallies)
  and reads `telemetry(i)` when rendering; ~10 parallel per-channel `Vec`s deleted.
- Two behaviour improvements fell out: a stopped runner's status receiver is kept
  until its thread exits, so ADR-018's final `Counters` lands and totals read exact
  at rest (previously the tail was dropped with the receiver); removed channels'
  runners land in an orphan bucket and are reaped, not silently detached.
- The status-bar "Errors" tally is now per-run (each channel's count resets when it
  starts, like the send counts and log tallies) instead of per-app-lifetime.
- `STATUS_QUEUE_CAP` moved to `core::supervisor` (re-exported to the GUI for the
  detail-header readout).

## ADR-020 — Stable channel identity (`ChannelId`)

**Status:** Accepted 2026-07-12 (external review round 2, "positional identity").

**Context:** Channels were identified everywhere by **slot index**: the runner
thread captured its index at spawn and stamped it into every `TalkerStatus` and
every structured `channel = n` tracing field; the GUI tallied per-channel log
counts in a positional `Vec` keyed by that field. Slots shift down when a channel
above is removed — but a running runner keeps its captured number, so after a
removal its log events (and its stale text, "channel 3 …") were attributed to
whichever row slid into its old position. The listener solved the same problem
with runtime-minted stable ids from day one; talker inherited the positional
scheme from its single-channel-era plumbing.

**Decision:** Identity and position are separated:

- **`core::channel::ChannelId`** — a process-unique, monotonic id (`u64`,
  1-based), minted by `TalkerSupervisor::push_slot` when the slot is created and
  carried by the slot for its whole life. Runtime-only: never persisted (profiles
  identify channels by position and name), never reused.
- **`RunnerIdentity { id, label }`** is what a runner is started with (ADR-019's
  `start` gained a `label` argument): the id goes into every `TalkerStatus`
  variant and every structured `channel` tracing field; the **label** — the
  custom channel name in quotes, else the 1-based position at start time — is
  used only in log *text*, frozen for the run (a rename shows on the next start).
  Attribution never rides the text; it rides the id.
- **The GUI keys its log tallies by id** (`HashMap<ChannelId, LogCounts>`) and
  maps row → id at render via `TalkerSupervisor::channel_id(i)`; removal drops
  exactly the removed channel's tally.
- **Positional indices remain for "which row right now":** supervisor slot
  methods, display-pane routing (`PayloadSample::slot` — the receivers travel
  with their slots, so the *current* index at drain time is correct), and the
  CLI's `--echo` tag (`ch0:` unchanged, via an id→position map).

**Consequences:**
- Log counts and error/status attribution can no longer land on the wrong row
  after a removal; a runner below a removed channel keeps counting into its own
  row. Pinned by `channel_ids_are_stable_across_slot_removal` and the id
  assertions in the runner status tests.
- Log text is unchanged in the common case ("channel 3 running") and better for
  named channels ("channel 'GPS' running"); supervisor lines before a first
  start fall back to the id form ("channel #4").
- `TalkerStatus` stays self-describing under the CLI's shared funnel with an
  identity that survives any future removal/reorder feature there.

## ADR-021 — Correlated command execution and applied runtime state

**Status:** Accepted 2026-07-12 (deep reliability review).

**Context:** ADR-019 reported whether a live interface or interval command entered
the runner queue, but the GUI immediately copied the requested interface into the
profile and treated it as applied. `UpdateInterface` can still fail while the runner
keeps its previous interface, and `SetInterval` can reject an invalid message index.
The UI could therefore claim runtime state that never took effect. A later unrelated
successful command also cleared the one shared command-error string, hiding the
unresolved failure. Listener's lifecycle work established the applicable rule:
observable state is reconciled from runtime facts, not from submitted intent.

**Decision:** Live mutations carry a process-unique `CommandId` and a scoped
`CommandTarget` (`Interface` or one message interval). The immediate supervisor result
is explicitly an enqueue outcome. After executing a command, the runner emits a
correlated `Applied` or `Failed` result on a dedicated reliable control channel,
separate from ADR-018's sampled/drop-and-count observer queue. The supervisor retains
each pending effect and accepts a completion only when its id and target match.

The start-time interface is likewise not considered applied until the runner reports
that `open` succeeded. A successful interface update replaces the supervisor's
applied-interface baseline; a failed update leaves the previous baseline intact and
states that fact in the channel banner. Command failures are retained independently
per target, and only a later successful command for that same target resolves one.
The supervisor commits an effect to its applied-runtime baseline only after the
correlated success; the profile remains desired/persisted state.

**Consequences:**
- Queue acceptance and runtime application are no longer conflated in APIs, comments,
  telemetry, or drift detection.
- Observer congestion cannot erase configuration truth. The reliable channel is sized
  for the start result plus the bounded command queue; its sends wake the GUI so it is
  drained promptly.
- Failed live interface edits remain visibly dirty against the confirmed live
  interface, even if the draft/profile is saved while the old interface keeps running.
- Results from stopped predecessors are drained only to let them finish; they cannot
  mutate the replacement runner's applied state.
- Tests pin failed-update retention, target-scoped error recovery, and start-time open
  confirmation.

## ADR-022 — Confirm whole-run state and reconfigure owned resources in place

**Status:** Accepted 2026-07-13 (post-ADR-021 reliability review).

**Context:** ADR-021 confirmed only the interface at start, while message drift still
used the profile as an ersatz applied baseline. The GUI consequently wrote message
drafts into the profile immediately after spawning a runner, before `open` succeeded.
The same review found two control/reopen gaps: a command accepted while the initial
open was blocking disappeared if that open failed, and opening a replacement before
dropping the old handle made same-port serial and explicitly bound UDP changes fail on
exclusive resources. These contradicted both ADR-021's runtime-truth rule and spec
§4.3's live parameter-change contract.

**Decision:**
- `TalkerSupervisor` retains the requested interface **and messages** as a pending
  start. `InterfaceOpened` promotes them together in one all-or-nothing supervisor
  state transition; an open failure promotes neither. GUI drift compares drafts with
  this confirmed whole run while live and with the profile only while stopped.
- While the supervisor remains active and its poll loop continues, an `Enqueued`
  command is not silently discarded: execution reports `Applied` or `Failed`, and
  `poll` converts unresolved pending ids to `Failed` when a runner exits or an explicit
  stop tears it down, including the initial-open race. Application teardown may
  abandon unobserved results because no observer remains.
- `Interface::reconfigure` handles resources that cannot be double-opened. Same-port
  serial settings are applied to the owned serial handle with rollback to the prior
  settings on error. UDP changes retaining the same local bind update socket options
  and destination in place, also with rollback. Different serial ports, different UDP
  local ports, and TCP continue to open a replacement first and swap only on success.
- Successful live interface and interval commands update `AppliedRunConfig` inside the
  supervisor. They do not mutate persisted profile state.

**Consequences:**
- A failed start cannot clear interface or message drift, and Save remains an explicit
  draft-to-profile operation rather than a side effect of runtime reconciliation.
- Same-port serial baud/parity/etc. edits and same-bound-port UDP destination/mode
  edits no longer fail merely because the runner correctly owns the old handle.
- The command-result lane now covers both execution rejection and runner exit before
  execution. Tests pin the exit race and same-bound-port UDP update.

## ADR-023 — Lossy fallback for unsupported code-page text

**Status:** Accepted 2026-07-13 (message-builder usability fix). The editor-geometry
portion is superseded by ADR-024; the lossy code-page fallback remains current.

**Context:** The ASCII payload editor accepts Unicode text, but a selected single-byte
code page cannot represent every Unicode scalar. Compilation previously rejected the
entire schedule when pasted text contained typographic punctuation, arrows, emoji, or
other unsupported characters. The same marker-aware editor also supplied a wrapping
layout job to an egui `TextEdit::singleline`; long messages therefore grew into many
visual rows and displaced neighboring controls.

**Decision:** Each character unsupported by the selected code page encodes as ASCII
`?` (`0x3F`), one replacement byte per Unicode scalar. The GUI shows an amber count
and UTF-8 recommendation beside the code-page selector and lists the affected
characters on hover. Unsupported source characters and only their resulting `?`
bytes in the wire preview and live Output pane receive a contrast-aware amber
background; literal question marks remain unmarked. The compiled message records
replacement byte positions once, and only the already rate-limited Output sample
carries that small provenance list, so the send hot path never rescans source text.
Light mode uses a pale amber background with dark text; dark mode retains the stronger
amber background with black text. Valid `‹XX›` markers continue to emit exact bytes,
while malformed marker syntax remains an error. The initial editor fix
also disabled soft wrapping inside a fixed-width, single-line editor; ADR-024 replaces
that geometry with bounded multiline editing.

**Consequences:** Pasted Unicode no longer prevents a channel from starting merely
because an ASCII code page cannot encode every character. Substitution remains
visible before and after transmission, and users needing exact Unicode or exact
arbitrary bytes can choose UTF-8/UTF-16 or byte markers/Hex. Tests pin the reported
example, both-theme contrast, end-to-end Output provenance, and the visual distinction
between fallback and literal question marks.

## ADR-024 — Bounded multiline message text editors

**Status:** Accepted 2026-07-13 (message-builder usability fix).

**Context:** No supported legacy single-byte code page can represent the full reported
set of typographic punctuation, arrows, and symbols. UTF-8 and UTF-16 can, but users
also need to compose intentional line-oriented payloads in any text format. A
single-line field concealed line feeds, while unbounded vertical growth or soft
wrapping could again displace the channel controls around a large message.

**Decision:** UTF-8 is the recommended format for preserving unrestricted Unicode;
it remains a message format rather than an entry in the ASCII code-page selector.
The UTF-8, UTF-16, and ASCII message editors are multiline and preserve each explicit
line feed in the compiled payload. They never soft-wrap. Each editor grows from three
visible rows up to eight as explicit lines are added, then scrolls vertically. A line
wider than the viewport scrolls horizontally. Both scrollbars appear only when their
axis overflows, and the outer editor width remains bounded so adjacent controls keep
their space. Horizontal extent comes from the same non-wrapping egui galley that the
`TextEdit` consumes; the editor reuses that precomputed galley for its first layout
request instead of separately summing every glyph on every repaint. Marker-aware
editors retain their repair snapshot as `Arc<str>` and replace it only when the text
changes, avoiding an additional full text copy on unchanged repaints.

All other message-derived GUI state is likewise revision-memoized. Every
wire-affecting editor change advances its `ScheduleDraft` revision; one
`MessageDraftAnalysis` build then converts and validates the message, computes ASCII
replacement provenance, and compiles/renders the Wire preview. Start eligibility,
drift detection, replacement warnings, and the preview consume that same analysis
until the revision changes. Hex preview formatting writes once into a pre-sized
string. Unchanged repaints therefore do no payload clone, compile, replacement scan,
or per-byte temporary-string allocation.

**Consequences:** Users can see and edit line-oriented messages without allowing a
large paste to take over the detail pane. Exact Unicode requires UTF-8 or UTF-16;
legacy code pages retain ADR-023's visible `?` substitution. Tests pin explicit line
feed encoding, no-soft-wrap layout, horizontal overflow, vertical growth, and the
eight-row viewport cap for both editor variants. They also pin reuse of the galley
that supplied the width, change-only replacement of marker repair snapshots, and
one analysis rebuild per changed revision rather than per repaint or consumer.

## ADR-025 — Preflight replacements before interrupting an active channel

**Status:** Accepted 2026-07-14 (active-run safety fix).

**Context:** A running channel with edited drafts always enabled `Apply & Restart`,
even when the replacement message could not compile. The GUI then stopped the healthy
runner before compiling its replacement. A transient edit such as an incomplete
`‹XX›` byte marker therefore took an active channel offline and reported the problem
only afterward as a zero-based `message 0` schedule error. The Wire preview reduced
the same useful error to `(message is incomplete)`.

**Decision:** A replacement follows a prepare-then-replace boundary. The candidate
interface, complete message list, and compiled schedule are built before
`TalkerSupervisor::start` performs the first runtime mutation. Any preflight failure
leaves the existing runner, applied configuration, telemetry, and Output state
unchanged. `Apply & Restart` uses complete draft validation and is disabled when that
candidate is invalid; its disabled tooltip and the message's Wire preview expose the
specific compile error. User-facing schedule diagnostics number messages from one.
Malformed byte-marker syntax remains an error rather than silently changing wire
bytes.

**Consequences:** Editing cannot interrupt transmission until a complete replacement
is ready. The supervisor still owns the valid restart and predecessor handoff, while
the GUI owns only draft preparation and presentation. Tests pin the disabled invalid
replacement state, exact one-based marker diagnostics, successful complete preflight,
and preservation of a real active UDP run across a rejected replacement.

## ADR-026 — Preparation has no live-time side effects

**Status:** Accepted 2026-07-15 (profile reliability and schedule timing follow-up).

**Context:** ADR-025 protected one active-channel replacement, but two neighboring
paths still crossed the commit boundary too early. GUI profile loading stopped every
runner before discovering that a hand-edited message could not compile or be
represented by the draft model. Schedule compilation also assigned `Instant`
deadlines during preflight, so time spent joining a predecessor or opening an
interface appeared as missed sends when the new runner finally began.

**Decision:** Preparation constructs inert candidates only. A profile load parses the
file, validates the complete profile, materializes every connection and message draft,
and round-trips those drafts through validated `ChannelConfig` construction. The same
draft-to-core conversions feed Save and Start. Only a successful
`PreparedProfileLoad` may stop runners and replace the workspace. Draft conversion
validates each `MessageConfig`, so malformed marker syntax cannot be saved or flushed
through another GUI path either.

`Schedule::compile_unarmed` compiles wire data and intervals without assigning clock
deadlines. Production entry points use that form; the runner calls `arm(now)` only
after it owns an open interface and immediately before entering the send loop. Arming
makes every active message due at that boundary and starts missed-send accounting
there. The timestamp-taking `Schedule::compile` remains a convenience for deterministic
scheduler tests and is implemented as compile-unarmed plus arm.

**Consequences:** A bad profile leaves the current workspace and active transmissions
untouched. Valid loads remain all-or-nothing at the UI commit point. Slow interface
opens and predecessor handoffs no longer age a candidate schedule, create false missed
sends, or move its first-fire grid. Tests pin complete profile preparation, rejection
before replacement, validation at draft conversion, and a deliberately delayed
unarmed schedule whose first poll sends immediately with zero misses.

## ADR-027 — Channel transport is chosen at creation

**Status:** Accepted 2026-07-15. **Context:** listener's established Add-template
model and the cross-app GUI harmonization following ADR-016 / listener ADR-019.

**Problem:** Talker's `+ Add` menu already chose Serial, UDP, or TCP, but the selected
channel header repeated those choices as three radio buttons. The second control was
not just visual duplication: changing a radio mutated the existing draft and queued
an immediate interface apply. A valid retained target configuration could therefore
replace a running channel's transport after one casual click. Listener instead treats
transport as structural: Add chooses a template and Configure edits its parameters.

**Decision:** Both GUIs use listener's model and one Add-menu order: UDP, TCP, Serial.
For talker, `ConnDraft`'s kind is creation-only and private outside its draft module;
it is set by `ConnDraft::new` or by materializing a profile interface. The detail pane
has no transport selector. Configure Connection renders and edits only the fields for
the existing kind. Runtime parameter reconfiguration remains unchanged.

**Consequences:** The normal workflow has one transport choice, and a running channel
cannot switch transport through an incidental header click. A different transport is
a different channel created through `+ Add`; messages are not silently transferred.
Profiles and the CLI still select a kind while constructing channels. There is no
profile-schema or runtime-protocol change. Focused tests pin the common menu order and
creation-time kind.

## ADR-029 — Live NMEA time is a compiled per-send template

**Status:** Accepted 2026-07-17 (spec v2.2).

**Context:** NMEA payloads were fully serialized during schedule compilation. A typed
GGA/RMC/ZDA time therefore stayed frozen across every send, unlike Talker's optional
prepended timestamp. Rebuilding all messages or moving protocol decisions into the
scheduler would weaken preflight and make the common static path pay for one dynamic
format.

**Decision:** `PayloadConfig::Nmea` gains additive, default-off `live_time` and
`live_time_millis` flags. Compiling a message produces either static bytes or an
immutable live-NMEA template containing parsed identity, typed fields, checksum mode,
and millisecond policy. `CompiledMessage::render_at(now)` uses one UTC instant for the
prepended timestamp and all live NMEA substitutions, then computes the outer checksum
over the resulting message.

The supported positions come only from `SentenceType::time_fields()` (nmea0183
ADR-028). UTC time is `hhmmss` or `hhmmss.sss`; RMC date is `ddmmyy`; ZDA day/month
are two digits and year is four. Typed fields are retained in the profile but replaced
on wire; a short field list is extended with empty fields through the last mapped
position. The NMEA checksum is rebuilt after substitution and still honors Correct,
Omit, and Wrong modes. Enabling live time for an unmapped standard or custom sentence
is a preflight error, never a silent static fallback.

**Consequences:** Static payloads remain pre-encoded and allocation behavior is
unchanged. A live NMEA send clones only that message's field vector, substitutes the
small explicit map, and serializes one sentence; a Criterion live-GGA case tracks this
cost beside an equal-wire-shape static GGA. The 2026-07-17 paired measurement was
~2.21 µs live versus ~140 ns static: below the workspace's action threshold until
roughly 4.5–5 kHz sustained live sends, so direct buffer rendering is deferred.
The GUI disables unsupported new selections, exposes the exact mapped fields, and
leaves an already-invalid selection enabled so the user can turn it off. Old profiles
deserialize with both flags false; the profile schema version is unchanged.

## ADR-031 — Payload compilation has one dynamic-safe public boundary

**Status:** Accepted 2026-07-17.

**Context:** `PayloadConfig::compile()` historically returned final `Vec<u8>` wire
bytes because every payload was static. After ADR-029, preserving that API required
rendering a live NMEA template at `Utc::now()` and returning a byte vector that looked
compiled but was already frozen. No production path used the method, but a future
caller could accidentally restore the stale-time defect ADR-029 removed.

**Decision:** Remove direct byte compilation from `PayloadConfig`. Payload parsing and
encoding stay private to the message module. The public path is
`MessageConfig::compile() -> CompiledMessage`, followed by `render()` for a real send
or `render_at()` for a deterministic preview/test. Static and dynamic payloads now
share one API whose type retains the distinction internally.

**Consequences:** Scheduled sends cannot lose live semantics by choosing an
apparently equivalent payload method. Encoding tests exercise the same message-level
compile/render boundary as production. This removes a pre-1.0 Rust API but changes no
profile field, wire output, GUI behavior, or schema version.

## ADR-032 — Send timing telemetry uses due deadlines and bounded cumulative histograms

**Status:** Accepted 2026-07-19.

**Context:** Missed-send totals identify cadence grid points that were skipped, but
they do not show near misses or distinguish scheduler wake delay from payload-render
and interface-call cost. `Schedule::poll` also rendered a dynamic payload before the
runner's retry-backoff gate, allocating bytes and sampling a timestamp even when that
due fire would be suppressed.

**Decision:** Scheduling and rendering are separate boundaries. `Schedule::poll`
returns `Due { index, scheduled_for }`, advances the same drift-free cadence grid, and
does not render. The runner records monotonic deadline lateness when it handles that
due result, applies retry suppression, and only then renders and calls the interface.
It records render duration and synchronous interface-call duration around those two
operations.

Each boundary uses a fixed-size cumulative histogram: bucket counts, sample count,
sum, and maximum, with no retained samples and no allocation while recording. The
three histograms travel in the existing cumulative `Counters` lane, so dropped
intermediate updates self-correct and the final blocking counters keep the completed
run exact at rest. The GUI shows a warm-up state, then p99 bucket upper bounds and
maximums. It labels those percentile values as `<=`, not exact observations.

**Measurement boundary:** deadline lateness begins at the message's monotonic cadence
deadline and ends after scheduler due selection. Render covers construction of the
wire payload, including dynamic timestamps. Send-call duration ends when the
application-level `Interface::send` returns. It does **not** prove that serial bits
left the adapter, a network packet left the kernel, or a peer received the bytes.

**Consequences:** A suppressed fire still contributes deadline telemetry but performs
no render, timestamp sample, allocation, or send call; tests pin the corresponding
sample-count identities. Per-fire collection remains constant-space and constant-time.
Cumulative p99 is deliberately coarse and can hide a recent change after a long run;
recent-window views, timer-mode reasons, capacity estimates, and export belong to the
staged telemetry follow-ups in `TODO.md`. No profile, wire format, timer-resolution
policy, or spec version changes in this decision.

## ADR-033 — Recent timing and timer policy are explicit bounded telemetry

**Status:** Accepted 2026-07-19.

**Context:** ADR-032's cumulative histograms preserve run-wide truth but eventually
make a recent slowdown almost invisible in the percentile. The GUI also could not say
which deadline-wait policy a runner was using, whether Windows accepted its 1 ms
request, or why that request was held. A queued interval change was applied after the
timer guard was reconciled; making the last fast message dormant could therefore enter
the indefinite idle wait while retaining the high-resolution request.

**Decision:** Each send-path histogram now has a companion recent view made from ten
fixed one-second segments. Recording remains allocation-free and constant-space.
Counter snapshots merge segments younger than ten seconds; no individual samples or
per-send telemetry events are retained. The GUI uses that approximate last-ten-second
view for p99 and keeps the cumulative deadline maximum as the run-wide outlier.

Timer policy is reconciled after queued commands and before schedule polling. The
refcounted Windows request records whether `timeBeginPeriod(1)` succeeded; only a
successful first request is paired with `timeEndPeriod(1)`, and nested holders share
the same outcome. `TimerStatus` reports the shortest active interval and one of
Standard, Windows 1 ms, Windows request failed, or native deadline waits. It travels
in cumulative counters and as a low-frequency edge update so an interval change can
refresh a channel that is about to become idle. Non-Windows targets do not make a
Windows-style resolution request and report their native deadline waits for fast
schedules.

**Consequences:** Current behavior and a past outlier are visible at the same time,
without telemetry work scaling with send rate or run length. A failed Windows request
is no longer presented as success or incorrectly lowered. Making a fast schedule
dormant releases the process request before the zero-wakeup idle block. At this
decision's boundary, high-resolution intent was not extended to slow schedules merely
because their payload printed milliseconds. ADR-034 resolves the follow-up with an
explicit channel mode instead of coupling cadence policy to output formatting. No
profile, wire-format, scheduler-cadence, or spec-version change was introduced by
ADR-033 itself.

## ADR-034 — Explicit Precise mode uses bounded deadline-resolution windows

**Status:** Accepted 2026-07-19 (spec v2.3).

**Context:** ADR-017's automatic Windows 1 ms request fixes high-rate schedules, but
a slow schedule can still wake several milliseconds late on the default Windows
timer tick. That jitter is visible when a payload prints milliseconds even though
the cadence is only 1 Hz. Timestamp formatting is not a reliable proxy for user
intent: a millisecond field may be informational, while a static payload may still
need a tighter trigger. Holding 1 ms resolution throughout every slow run would pay
the power cost while almost all of each interval is idle. Wall-clock phase alignment
is a separate semantic choice and must not be implied by a timer-resolution control.

**Decision:** `ChannelConfig` gains an additive, default-Standard `TimingMode` with
Standard and Precise choices. It is a run-fixed setting: GUI edits are prepared and
applied through Apply & Restart, and CLI profile runs pass the same value to the
runner. Standard preserves ADR-017 exactly. Any active schedule whose shortest
interval is below 32 ms continues to hold the Windows request continuously in either
mode.

For a slower Precise schedule on Windows, each runner uses a two-stage interruptible
wait. It first waits until `deadline - 32 ms` without a request, then acquires the
existing process-refcounted RAII guard for the final `recv_deadline`. The guard is
released as soon as the deadline is due, before dynamic rendering or the synchronous
send call. Commands interrupt both stages; changed deadlines are recomputed on the
next loop. Nearby windows naturally overlap through the process refcount, and a
dormant schedule reaches its indefinite command wait with no guard held. On
non-Windows targets, Precise deliberately remains one native deadline wait with no
extra staging wake.

Timer telemetry carries both configured mode and active reason (`HighRate` or
`PrecisionWindow`). A Windows request outcome remains stable between precision
windows so the GUI does not flicker back to Standard. Repeated per-window OS-call
logging is suppressed; a failed request is warned on the state transition and stays
visible in telemetry.

**Consequences:** Slow Precise schedules spend only their final deadline windows at
requested 1 ms resolution; a lone 1 Hz deadline is approximately a 3.2% duty cycle,
while close or interleaved deadlines can make the effective duty cycle higher. There
is no spin wait and no dedicated timing thread. Precise can reduce Windows wake
quantization, but cannot remove scheduler load, rendering cost, blocking interface
cost, or hardware/driver buffering. `TimingMode` does not itself align sends or make
`SystemTime` more accurate; ADR-037 adds alignment as a separate explicit choice.
The new profile field is backward-compatible through `serde(default)`, so profile
schema version 2 remains unchanged.

## ADR-035 — Capacity preflight separates physical proof from measured estimates

**Status:** Accepted 2026-07-19 (spec v2.4).

**Context:** Timing percentiles and observed throughput explain an active run, but a
user should see obvious capacity limits before Start. Serial provides enough facts
for a physical sustained-rate calculation; UDP and TCP do not expose the path's link
capacity. Application render/send timing can estimate runner service headroom, but
the two boundaries are separate histograms and synchronous `send` commonly ends at a
driver or kernel buffer rather than at the wire. Treating either estimate as a hard
real-time admission test would overstate what Talker knows and prevent intentional
overload testing.

**Decision:** `core::capacity` owns pure, GUI-independent calculations. Channel draft
demand sums each non-dormant message's exact compiled wire bytes divided by interval.
The GUI stores that byte count beside its memoized fixed-time preview, so selected-
channel repaint folds scalar values only; it never recompiles or rerenders payloads.
If any message is incomplete or invalid, the aggregate is withheld rather than
silently computed from a partial schedule.

For Serial, one byte consumes one frame of `1 start + configured data + parity +
stop` bits. Required bits per second are compared with baud. Greater than 100% is a
physical sustained-rate impossibility; 80% through 100% is highlighted as low
margin. Flow control is not credited as capacity because it can only pause output.
UDP/TCP report requested messages and bytes per second without an invented link
limit.

Measured application headroom requires 20 paired render/send observations. The
separate p99 histogram upper bounds are added and divided into the draft's aggregate
message rate. A sufficiently populated recent ten-second view wins; otherwise the
cumulative run supports slow schedules. The readout calls this a sum of p99 bounds,
not a joint p99, and warns when estimated utilization reaches 80%. Tooltips state the
buffering, aligned-burst, stale-draft, and physical-wire limits.

All findings are advisory and never join Start blockers. A regression test pins that
an obviously oversubscribed serial draft remains startable. Existing draft-interface
materialization is shared by drift and capacity checks, avoiding a second per-frame
string-owning config build.

**Consequences:** Users can distinguish an impossible sustained serial request from
an application-side estimate and see both before and during a run. Actual misses,
failures, suppression, lateness, and throughput remain the verdict. The model is
constant-space, performs no send-path work, and introduces no profile or wire-format
change. A future per-message or phase-aware schedulability analysis may model aligned
bursts more tightly, but it must remain separate from this aggregate capacity view.

## ADR-036 — Completed runs retain one exact, self-contained summary

**Status:** Accepted 2026-07-19 (spec v2.4.1).

**Context:** Live cumulative counters are exact at rest, but a new start deliberately
resets them. That made before/after timing comparisons and issue reports depend on
manual transcription before restarting. A restart can also finish an old runner's
tail after a newer run, so retaining whichever completion happened to be polled last
would be incorrect. Building an export string continuously would add pointless work
to the selected-channel repaint path.

**Decision:** Each runner receives a process-unique, increasing `RunId`. It records a
wall-clock start beside the monotonic instant that arms the schedule, then records a
wall-clock finish and monotonic elapsed duration when its command loop ends. After
attempting delivery of ADR-018's blocking final Counters, the runner emits one
`RunFinished` on the reliable control lane from the same final counter/timing
snapshot. The bounded lane reserves room for start-open truth, every simultaneously
queued command completion, and this one completion record.

`RunSummary` owns the channel/run identity, label, start/finish time, monotonic
elapsed duration, end reason, sent bytes/messages and per-message counts,
failed/suppressed/missed outcomes, observer drops, cumulative/recent bounded timing,
and final timer policy. The supervisor retains only the greatest `RunId` per stable
channel slot. During Apply & Restart, stale sampled status from the predecessor is
discarded so it cannot contaminate replacement telemetry, but its reliable control
tail remains drainable until the self-contained summary arrives. Loading a profile
creates fresh slots and therefore clears retained summaries.

The selected-channel GUI shows one collapsed "Last completed run" block. Its heading
contains elapsed, sent, and unsent totals; expansion shows final times, outcomes,
timer/timing facts, and build/platform facts. `Copy summary` produces a versioned,
line-oriented text report only on click. It does not perform file I/O, and no report
string is built during an ordinary repaint.

**Consequences:** One completed run remains available through normal stop/restart
cycles without unbounded history or per-send work. Retention is process-local and
bounded to one summary plus one per-message count vector per channel. Wall-clock
timestamps remain subject to system clock steps; elapsed duration is monotonic. An
interface-open failure occurs before a send run is armed and therefore does not
replace the last completed-run summary. CLI behavior, profiles, wire bytes, and
scheduler cadence are unchanged.

## ADR-037 — UTC phase alignment anchors once, then advances monotonically

**Status:** Accepted 2026-07-19 (spec v2.4.2).

**Context:** Precise mode improves when a deadline wait wakes, but some test sources
also need their application send calls phased to recognizable UTC boundaries. Making
that behavior implicit in Precise would conflate wake mechanics with schedule
semantics. Scheduling directly from `SystemTime` every interval would also expose
cadence to clock slew and steps, while replaying elapsed boundaries after a forward
step would create the same harmful catch-up bursts the stall policy rejects.

**Decision:** `ChannelConfig` gains an additive, default-Immediate
`CadenceAlignment`. Immediate preserves the existing first-send-at-arm behavior.
`UtcPhase` computes each active message's strict next boundary by Unix-epoch modulo
that message's interval. Exactly on a boundary waits one complete interval. The
first boundary is mapped to `Instant`; subsequent deadlines advance from the prior
monotonic grid, so ordinary wall-clock slew does not accumulate cadence drift and
intervals need not divide a day.

An aligned runner compares a paired wall/monotonic anchor with `SystemTime` at most
once per second. A displacement of at least 250 ms rebases only future deadlines to
their next UTC phases, increments a telemetry counter, and never replays past grid
points or adds scheduler misses. A live nonzero interval change uses the new
interval's strict next phase. Immediate mode performs no periodic wall-clock check.
Checked deadline arithmetic turns an unrepresentable extreme interval into an
unscheduled deadline instead of panicking.

The GUI exposes alignment beside Timing and applies a change through the existing
Apply & Restart path. Timer status and completed-run reports name the configured
alignment and wall-clock rebase count. The setting does not compensate for rendering,
synchronous send, kernel/driver buffering, or physical serialization.

**Consequences:** Independent channels can share a UTC phase without a central clock
thread, spin wait, or per-send wall-clock syscall. Default profiles and immediate
startup are unchanged; `serde(default)` plus omission of Immediate keeps profile
schema version 2. A wall-clock step deliberately moves future aligned sends, while
elapsed timing and each between-step cadence remain monotonic.

## ADR-038 — Process timer-resolution mechanics live in `wiredata-timing`

**Status:** Accepted 2026-07-19 (spec v2.4.2).

**Context:** ADR-017 and ADR-034 originally kept the Windows timer guard inside
Talker. Listener's exact Idle deadlines now need the same refcounted
`timeBeginPeriod(1)` lifetime and minimized-window throttling policy. Copying this
process-wide state into both binaries would duplicate the lock/call ordering and make
the two implementations drift. This is narrower than the general `wiredata-core`
runtime crate rejected in ADR-016: no schedules, config, telemetry, or application
types need to be shared.

**Decision:** Add the internal, non-published `wiredata-timing` crate. It owns only
the refcounted RAII `HighResolutionGuard`, the Windows 1 ms begin/end calls, their
success state, and the one-time process throttling opt-out. Talker retains threshold,
window, intent, and telemetry policy in `core::timing`; Listener retains its Idle
deadline policy in its runtime. On Linux and macOS the guard preserves the same RAII
shape but performs no resolution call and native deadline waits remain unchanged.
Poisoned lock recovery and a defensive holder underflow check keep release behavior
bounded; a failed first Windows request is shared by nested holders and is never
paired with an end call.

**Consequences:** Talker and Listener cannot issue conflicting process timer-period
lifetimes, and Windows feature dependencies have one owner. The crate is intentionally
not a home for histogram helpers, schedulers, wall-clock alignment, or GUI state.
macOS App Nap is a separate activity-policy question, not a timer-resolution analog.

## ADR-039 — Bounded duration telemetry primitives live in `wiredata-telemetry`

**Status:** Accepted 2026-07-20.

**Context:** Talker's send telemetry and Listener's receive telemetry came to use
byte-identical duration buckets, cumulative histogram arithmetic, and ten rotating
one-second recent-window segments. ADR-027 in Listener originally kept that helper
local because there was not yet enough reuse to justify another crate. Once both
applications became durable consumers, retaining two copies made numeric fixes and
segment-aging corrections liable to drift silently. `wiredata-timing` remains the
wrong owner because its scope is process-wide OS timing mechanics, not measurements.

**Decision:** Add the internal, non-published, dependency-free
`wiredata-telemetry` crate. It owns `DurationHistogram`, the fixed duration bucket
boundaries, `RecentDurationHistogram`, its private timed segments, and the bounded
ten-second aging/merge algorithm. Talker and Listener re-export the cumulative type
through their existing telemetry modules so their application-facing paths do not
change.

Only generic numeric storage and aging are shared. Talker retains
`SendTimingTelemetry`, `SendTimingReport`, its recorder and measurement boundaries.
Listener retains byte/chunk histograms, transport and timer summaries, pipeline
measurement boundaries, completed-run retention, and presentation. Timer-resolution
mechanics remain solely in `wiredata-timing`.

**Consequences:** Bucket-boundary and recent-window fixes now have one implementation
and one focused test suite, while recording remains allocation-free and constant
space. The extra crate does not create a general shared runtime or shared application
telemetry schema. This decision supersedes only ADR-038's exclusion of histogram
helpers and the local-implementation portion of Listener ADR-027; their remaining
scope and measurement decisions stand. No profile or wire format changes.

## ADR-040 — Timer lifecycle is explicit; Precise windows stay bounded

**Status:** Accepted 2026-07-20.

**Context:** A review questioned whether ADR-034's final-window policy changes
`timeBeginPeriod`/`timeEndPeriod` too often near the 32 ms high-rate threshold: a
50 ms cadence stages for 18 ms and can make about twenty matched request/release
pairs per second. The proposed remedy was to hold resolution continuously whenever
the 32 ms window occupied at least half the interval. That would replace known time
outside the higher-resolution policy with an unmeasured reduction in API-call churn.
Microsoft documents that multiple matched calls are supported and advises requesting
resolution immediately before timer use and releasing it immediately afterward
([`timeBeginPeriod`](https://learn.microsoft.com/en-us/windows/win32/api/timeapi/nf-timeapi-timebeginperiod));
its
[energy guidance](https://learn.microsoft.com/en-us/windows-hardware/test/assessments/results-for-the-idle-energy-efficiency-assessment)
likewise says to restore the lower frequency when the precise task completes. The
runner separately spread guard acquisition, staging, due-time release, final release,
status derivation, notification, counter invalidation, and observer-drop accounting
across a large send loop and positional helper calls.

**Decision:** Retain ADR-034's bounded policy. Any schedule whose shortest active
interval is strictly below `HIGH_RATE_THRESHOLD` (32 ms) holds the shared request
continuously in either timing mode. A Precise schedule at or above that threshold
uses the final `PRECISION_WINDOW`; Standard uses its native wait. The process-wide
refcount means an overlapping holder naturally prevents redundant underlying OS
transitions. Expanding continuous holding without measured evidence is rejected.

Independently, the runner encapsulates timer lifecycle in a `TimerReconciler`. It
owns the current intent, guard, derived status, timer-edge notification endpoints,
counter refresh instant, and observer-drop count. Schedule reconciliation, wait
preparation, bounded-window due release, and final release are explicit methods. An
interrupted precision-window wait retains its guard until the recomputed wait plan
decides whether the deadline is still inside the window; a transition from continuous
to windowed releases immediately. The guard is always dropped before rendering and
sending for a windowed cadence and before final blocking status delivery. A named
`TimerStatusInput` groups the status derivation context instead of another positional
argument list.

**Consequences:** A 50 ms Precise run deliberately releases its guard for the first
18 ms and holds it for the final 32 ms of each interval when no other runner holds the
process request. This minimizes known high-resolution duty time; API-call overhead can
be revisited only with timing or energy evidence. Dormant schedules still release
before their indefinite wait. Linux and macOS still use one native deadline wait and
make no platform timer request. Isolated tests pin high-rate continuous retention,
near-threshold staging, due/idle release, timer-edge notification, counter
invalidation, and drop accounting. The Talker specification now incorporates the
reconciler without changing profiles, wire formats, or ADR-034's wake policy.

## ADR-041 — Diagnostics lead with decisions without hiding telemetry

**Status:** Accepted 2026-07-20.

**Context:** The selected-channel header exposes exact delivery outcomes, cadence
timing, timer policy, draft capacity, measured service estimates, throughput, and
observer pressure. That evidence is useful for investigation and support reports,
but its flat density makes the first operational question—whether anything currently
needs attention—slower to answer. Replacing the readouts with a generic health score
would be worse: it would conceal which boundary was measured, collapse unavailable
and warm-up states into a number, and overstate what buffered host-side measurements
can prove.

**Decision:** Add a compact, decision-oriented diagnostics summary with three
Talker-owned rows: **Send outcomes**, **Cadence**, and **Capacity**. Each row presents
a short assessment and the most relevant existing evidence. Send outcomes derive from
scheduled, locally accepted, failed, suppressed, and missed outcomes, and explicitly
do not claim physical-wire or peer delivery; Cadence derives from the existing
deadline/timer observations; Capacity derives from the current draft demand, serial
utilization where applicable, and measured service estimate when warmed up. Unknown,
unavailable, stale, and warm-up states remain explicit rather than being treated as
healthy.

An **Attention** callout appears only for derived exceptions that merit operator
notice. It is a presentation of existing state, not a persistent green status and not
an opaque composite health score. Row severity and callout selection may prioritize
the most actionable evidence, but the application owns those rules and their
thresholds, and the absence of a callout means only that no configured exception was
derived from the available evidence. Complete telemetry, measurement boundaries, and
caveats remain available under collapsed details; the compact rows do not replace or
weaken them.

`wiredata-ui` may own only the identical egui card, row, and callout
chrome shared with Listener. Talker owns the row labels, evidence selection,
wording, severity mapping, and thresholds. This is a view-model and layout change
only: runtime behavior, telemetry collection and types, retained summaries, clipboard
reports, wire output, and profiles are unchanged. Listener makes the paired but
receive-specific decision in listener ADR-033.

**Consequences:** The default view answers the three common send-side decisions with
less scanning while every underlying fact remains inspectable and exportable. The
design stays auditable because an operator can expand the evidence behind a derived
exception, and it cannot imply an all-clear from missing data. Pure application-side
classification tests can pin row and attention behavior without coupling shared
chrome to Talker semantics or adding work to the send path.

## ADR-042 — Pushed timing snapshots carry freshness and final provenance

**Status:** Accepted 2026-07-27.

**Context:** Talker's counter lane is deliberately push-based and send-path driven.
At a slow or dormant cadence, the latest collapsed recent-window histogram can remain
unchanged long after its samples would have aged out had the runner computed another
snapshot. The GUI nevertheless preferred a recent histogram after warm-up for
application headroom and displayed it in Cadence and detailed Timing. A retained
sample count alone cannot establish that this evidence is current.

Adding a periodic telemetry heartbeat would refresh aging, but it would also discard
the runner's intentional zero-wakeup dormant behavior. Inferring finality from
whether the UI considered a runner stopped was also insufficient: a final snapshot
can arrive before its thread is reaped, while an abnormal exit can stop without
delivering the mandatory final update.

**Decision:** Every `TalkerStatus::Counters` update carries the exact monotonic
`captured_at` instant passed to the timing snapshot computation. Periodic updates
carry `final_snapshot = false`; the mandatory exact-at-rest update uses the run's
existing final monotonic instant for both snapshot computation and `captured_at` and
carries `final_snapshot = true`. The supervisor retains both values beside the
collapsed recent timing. Apply & Restart resets this telemetry and drops the
predecessor's sampled-status receiver, so an old run's final counter tail cannot
overwrite its replacement; the predecessor's self-contained completion summary
continues on the separate reliable control lane.

Talker classifies the retained pushed snapshot once for all GUI consumers:

- no capture instant is **Pending**;
- a non-final snapshot younger than `RECENT_WINDOW` is **Current**, with its capture
  age available for presentation;
- a non-final snapshot whose age is greater than or equal to `RECENT_WINDOW` is
  **Expired**; and
- an explicitly provenance-marked snapshot is **Final**, regardless of later
  display age.

Capacity, Cadence, and detailed Timing consume that same classification. Expired
recent histograms are neither presented as current nor used for measured application
headroom. Capacity may fall back to sufficiently warmed cumulative run-wide timing
and labels that source explicitly. Final snapshots remain exact-at-run-end evidence.
If a runner exits abnormally before sending its final update, its last periodic
snapshot remains non-final and can expire rather than being misrepresented as exact
at rest.

No heartbeat is added. Periodic Counters remain rate-limited emissions on the send
path, plus the one mandatory final update. An all-dormant schedule continues to
block indefinitely on its command receiver and performs no telemetry wake.

**Alternatives considered:**

- **Emit a periodic snapshot heartbeat:** rejected because it introduces runner
  wakeups solely for presentation and weakens the dormant zero-wakeup contract.
- **Treat every retained recent histogram as usable until another update arrives:**
  rejected because sample count does not encode freshness and can overstate or
  understate headroom.
- **Infer finality from stopped/draining thread state:** rejected because observer
  lifecycle and measurement provenance can transition at different instants and an
  abnormal exit may have no exact final snapshot.
- **Treat partially aged snapshots as predictably conservative:** rejected because
  retained older samples may raise or lower a percentile; the bias has no guaranteed
  direction. `RECENT_WINDOW` is used only as the exact full-expiry boundary.

**Consequences:** Slow and dormant schedules retain zero-wakeup operation while the
GUI can distinguish current evidence, fully expired pushed evidence, and exact final
evidence. All three timing-driven diagnostic areas agree on the source and state;
run-wide fallback remains historical evidence and is labelled as such. The added
fields change Talker's public Rust observer-protocol shape, although they remain
process-local metadata. There is no persisted or serialized telemetry schema,
profile schema, clipboard-report format, wire-data, cadence, or interface behavior
change.

---

## ADR-043 — Telemetry freshness follows the observation model, not the app

**Status:** Accepted 2026-07-27.

**Context:** ADR-042 gives Talker's pushed counter lane an explicit capture instant
and freshness classification. Listener surfaces comparable timing telemetry built on
the same bounded primitives (ADR-039; listener ADR-032) and carries no such instant.
The difference is deliberate, but nothing recorded it, so it reads as an omission on
one side or the other.

It follows from the two runtime models (ADR-002 versus listener ADR-001). Talker has
no async runtime: its runner owns the send loop and **pushes** collapsed snapshots
from the send path, so a retained snapshot ages between emissions — indefinitely on a
dormant schedule. Listener is Tokio-hybrid: a snapshot request is answered inside the
channel's own task, and every recent window is collapsed at the moment it is served,
so a served window always ends at the request.

**Decision:** Freshness handling belongs to the observation model, not to the
telemetry. Talker retains snapshots across time, so Talker proves their age
(ADR-042). Listener computes on demand, so Listener adds no capture instant — under
pull, a stale window is not representable and the field would always read "now".

Neither mechanism is ported across. In particular, Talker does not adopt
compute-on-demand for its counter lane: answering a poll would require waking a
dormant runner, which is precisely the zero-wakeup contract ADR-042 preserves by
rejecting a heartbeat.

The two panels stay consistent in **vocabulary** rather than mechanism — shared
warm-up wording, bounded-percentile notation, and `RECENT_WINDOW`. Talker names an
*expired* state because it can have one; Listener does not.

The general rule, for any lane added later in either app: whichever side retains a
collapsed snapshot across time owns proving its age.

**Consequences:** The asymmetry is recorded on both sides, so neither app acquires an
always-"now" capture instant nor a telemetry heartbeat by analogy with the other. No
profile schema, clipboard-report format, wire-data, cadence, or interface behavior
changes. See listener ADR-035 for the same decision from Listener's side.

---

## Open questions

The following decisions are deferred until the relevant module is written. They are recorded here so they are not forgotten and so the eventual decision (in a future ADR or commit) can reference the context.

**OQ-1 — `nmea0183` path-vs-version dependency.** The `talker` crate currently depends on `nmea0183` via `{ path = "../nmea0183" }`. When `nmea0183` is published to crates.io (per ADR-001), this should become `{ path = "../nmea0183", version = "0.1" }` so that downstream consumers building against published versions resolve cleanly while in-workspace builds continue to use the local source. Defer until publication is imminent.

**OQ-2 — `toml` 1.x vs 0.8 API. — Resolved (v2.0).** `core::profile` was implemented against `toml = "1"` with no friction: `Profile::load` parses to a `toml::Value` to inspect the schema version before full deserialization, then `toml::from_str` / `toml::to_string_pretty` handle the round trip. The 1.x API surface was sufficient; the fallback to `"0.8"` was not needed. The workspace stays on `toml = "1"`.

**OQ-3 — `nmea0183` serde feature activation in `talker`. — Resolved (v2.0).** `core::profile` uses a **`talker`-side representation**: an NMEA message stores plain-string talker, sentence type, and fields plus Talker-owned checksum/live-time flags, not serialized `nmea0183` types. Those strings are parsed into library types at message compilation; a static sentence is serialized then, while ADR-029's live template serializes per send. The profile schema is therefore decoupled from the library's struct shapes, and the `talker` dependency on `nmea0183` does **not** enable the `serde` feature (`nmea0183 = { path = "../nmea0183" }`). The `serde` feature on `nmea0183` itself still exists and is still verified to compile, for the benefit of other potential consumers.

**OQ-4 — `nmea0183` library MSRV policy.** Moved to [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) — it concerns the library's publication policy. See ADR-008 above for the workspace MSRV context it builds on.

New open questions should be added here as they arise during implementation.
