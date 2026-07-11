# Architecture Decision Record — Talker
**Project:** talker  
**Version:** 1.5  
**Date:** 2026-05-22  
**Status:** Accepted

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

**Decision:** The project is structured as a Cargo workspace, initially with `talker` (binary) and `nmea0183` (library). The `nmea0183` crate has no dependency on `talker` and no knowledge of its internals. The workspace now also contains `listener`, a sibling receive crate; it originally shared `nmea0183` but **dropped that dependency in its v2.0 stream-only pivot** (listener ADR-010 — no decoding), so today `nmea0183` is consumed only by `talker`.

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
- Talker's local visuals/font installers were replaced by `wiredata_ui::{fonts::install_fonts, style::install_visuals, style::apply_style_tweaks}`. Talker's light theme is now listener's grey-backdrop look; talker's original dark values seeded the shared dark theme. The dark/light **toggle** remains talker-only until listener grows one.
- Talker also adopts listener's window-startup lessons: `persist_window: false` (no post-show geometry jump) and a minimum window size.
- Listener's `gui/{fonts,theme}.rs` and `widgets/format.rs` became thin re-exports, so its call sites are unchanged.
- The crate has no `docs/` folder; its decisions live in the two app ADR series (this entry and listener ADR-019).

---

## ADR-017 — High-resolution OS timer scope for high-rate schedules

**Context:** The runner waits for its next fire with a deadline-bounded blocking receive (`recv_deadline`, ADR-002). On Windows a parked thread with a timeout wakes on the system scheduler tick — **15.625 ms** by default — so a 10 ms interval (100 Hz) can never be honored: every wake lands more than one interval late, the stall policy (spec §8.1) skips the backlog to stay on grid, and ~36% of sends are skipped with ~15.6 ms observed spacing. 10 Hz absorbs the same jitter invisibly, which is why the problem only appears at high rates.

**Options:** (a) request 1 ms resolution process-wide for the whole app lifetime — simple but pays an idle power cost and is against Microsoft guidance; (b) hybrid wait (sleep short, spin the last ~2 ms) — precise to sub-ms but burns a core per fast channel; (c) request 1 ms resolution **only while a schedule actually needs it**.

**Decision:** (c), in `core::timing`. A refcounted RAII guard (`high_resolution()` / `HighResolutionGuard`) wraps `timeBeginPeriod(1)`/`timeEndPeriod(1)`: the first holder raises the request, the last drop releases it, and the count and OS call share one lock so concurrent acquire/release can't reorder the pair. Each runner re-evaluates `Schedule::min_active_interval() < HIGH_RATE_THRESHOLD` (32 ms = two default ticks) every loop pass, so `SetInterval` acquires/releases mid-run. Both entry points (GUI funnel, CLI run) additionally opt out of Windows 11's minimized-window timer throttling via `SetProcessInformation(ProcessPowerThrottling, IGNORE_TIMER_RESOLUTION)` — otherwise a minimized long soak silently falls back to 15.6 ms wakes. Everything is a no-op off Windows.

**Consequences:**
- 100 Hz sends hit cadence (wake jitter ~1–2 ms ≪ 10 ms interval); the practical ceiling moves to roughly 500 Hz–1 kHz, beyond which option (b) would be needed.
- No elevation required; per-process since Windows 10 2004, so other processes are unaffected. The kernel releases the request on process death (any kind), so a leaked guard cannot outlive talker — the RAII release is about dropping the power cost early, not correctness.
- The GUI's "Missed sends" readout is the acceptance signal: it should stay 0 at 100 Hz on an otherwise healthy interface.

---

## Open questions

The following decisions are deferred until the relevant module is written. They are recorded here so they are not forgotten and so the eventual decision (in a future ADR or commit) can reference the context.

**OQ-1 — `nmea0183` path-vs-version dependency.** The `talker` crate currently depends on `nmea0183` via `{ path = "../nmea0183" }`. When `nmea0183` is published to crates.io (per ADR-001), this should become `{ path = "../nmea0183", version = "0.1" }` so that downstream consumers building against published versions resolve cleanly while in-workspace builds continue to use the local source. Defer until publication is imminent.

**OQ-2 — `toml` 1.x vs 0.8 API. — Resolved (v2.0).** `core::profile` was implemented against `toml = "1"` with no friction: `Profile::load` parses to a `toml::Value` to inspect the schema version before full deserialization, then `toml::from_str` / `toml::to_string_pretty` handle the round trip. The 1.x API surface was sufficient; the fallback to `"0.8"` was not needed. The workspace stays on `toml = "1"`.

**OQ-3 — `nmea0183` serde feature activation in `talker`. — Resolved (v2.0).** `core::profile` uses a **`talker`-side representation**: an NMEA message is stored as `PayloadConfig::Nmea { talker: String, sentence_type: String, fields: Vec<String> }` — plain strings, not `nmea0183` types — and converted to a `nmea0183::NmeaSentence` only at compile time. The profile schema is therefore decoupled from the library's struct shapes, and the `talker` dependency on `nmea0183` does **not** enable the `serde` feature (`nmea0183 = { path = "../nmea0183" }`). The `serde` feature on `nmea0183` itself still exists and is still verified to compile, for the benefit of other potential consumers.

**OQ-4 — `nmea0183` library MSRV policy.** Moved to [`nmea0183/docs/ADR.md`](../../nmea0183/docs/ADR.md) — it concerns the library's publication policy. See ADR-008 above for the workspace MSRV context it builds on.

New open questions should be added here as they arise during implementation.
