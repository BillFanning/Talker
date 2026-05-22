# Architecture Decision Record — Talker
**Project:** talker  
**Version:** 1.5  
**Date:** 2026-05-22  
**Status:** Accepted

---

## What belongs in an ADR

An ADR captures *why* a significant decision was made, not just *what* was decided. It records the context, the options considered, the choice made, and the consequences — so that anyone joining the project later (or the original author six months later) can understand the reasoning without reconstructing it from scratch. An ADR is not a specification; it complements the spec by explaining the decisions that shaped it.

---

## ADR-001 — Workspace structure: two crates, not one

**Context:** The NMEA 0183 module was identified early as reusable across other projects. The question was whether to keep it as a module inside the `talker` binary or make it a separate library crate.

**Decision:** The project is structured as a Cargo workspace with two members: `talker` (binary) and `nmea0183` (library). The `nmea0183` crate has no dependency on `talker` and no knowledge of its internals.

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
- `crossbeam-channel` `select!` macro is used in each talker thread to wait on both the schedule timer and incoming command channels simultaneously without spinning.
- Each active connection has its own dedicated channel pair with the UI thread.
- `core::connection` manages a collection of connection instances from the initial implementation; there is no single-connection shortcut to be refactored later.
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
- `talker`'s `core::data` module uses `crc` for the general checksum feature.
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

**Context:** NMEA 0183 has ~36 standard talker IDs and many sentence types. New proprietary sentences (`$P...`) are encountered regularly in marine and survey equipment.

**Decision:** 
- Standard talker IDs are represented as an enum with a `Custom(String)` variant for arbitrary two-character IDs.
- Sentence types follow the same pattern: an enum with a `Custom(String)` variant.
- Proprietary sentences use a dedicated `ProprietarySentence` type with named variants for known formats (`Prdid`, `Pashr`) and a `Raw` variant for arbitrary `$P` sentences.

**Consequences:**
- Named proprietary sentences (`$PRDID`, `$PASHR`) get field-level construction and validation.
- The `Raw` variant accepts any manufacturer code and comma-separated field string with optional checksum — no validation beyond checksum computation.
- The `$PASHR` GNSS quality field (field 10) is exposed as a raw `u8` rather than an enum, because Trimble and Novatel define the values differently. The crate documentation must record both vendor conventions explicitly.
- `$PRDID` does not include a checksum by protocol convention; the builder must not append one.

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

## ADR-011 — CLI multi-connection model

**Context:** The GUI supports multiple simultaneous connections as a first-class feature. The question was whether CLI mode should be one connection per process or support multiple connections in one process.

**Decision:** CLI mode supports multiple simultaneous connections in a single process, using the same `core::connection` collection and per-connection talker thread model as the GUI. The primary mechanism is `--profile`, which may define one or many connections. Ad-hoc multi-connection via repeated CLI flags is deferred.

**Alternatives considered:**
- One connection per CLI instance: Simple to implement, but requires users to manage multiple terminal sessions and processes for multi-port work. Inconsistent with the GUI model and defeats the profile system. Rejected.
- Repeated `--connection` flags for ad-hoc multi-connection: Desirable long-term but adds CLI parsing complexity. Deferred to a future iteration; `--profile` covers the primary use case.

**Consequences:**
- `talker --profile <name>` is the canonical way to launch multi-connection sessions from the CLI.
- The CLI and GUI share identical `core` behavior for connection management. There is no CLI-specific connection limit or shortcut.
- stdout echo in multi-connection CLI mode outputs data from all connections interleaved. Each line is prefixed with a connection identifier to allow filtering.

---

## ADR-012 — Binary field types

**Status:** Superseded by the spec v2.0 message-format model (see ADR-015 context and CLAUDE.md "Message Formats"). Spec v2.0 removed the typed-binary-field concept: arbitrary byte sequences are now entered in **Hex** format, and structured data is built through the UTF-8/UTF-16/ASCII/NMEA formats. `core::data` and the `BinaryField` enum were never carried into the v2.0 codebase. This ADR is retained for historical context.

**Context:** The spec originally deferred the exact set of binary field types. A concrete decision is needed before `core::data` can be implemented.

**Decision:** Binary data is constructed as an ordered sequence of typed fields. Supported types are: `u8`, `u16`, `u24`, `u32`, `u64`, `i8`, `i16`, `i32`, `i64`, `f32`, `f64`, and raw bytes (arbitrary hex). Byte order is selectable per field — big-endian (default) or little-endian.

**Rationale for `u24`:** Three-byte unsigned integers appear frequently in sonar, audio, and oceanographic equipment. Without `u24`, users must construct them manually from raw bytes, which is error-prone. The implementation cost is low.

**Rationale for big-endian default:** The majority of marine and survey instruments use big-endian (network byte order). Defaulting to big-endian reduces misconfiguration for the primary target audience.

**Consequences:**
- `core::data` implements a `BinaryField` enum with one variant per type plus `RawBytes(Vec<u8>)`.
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

## Open questions

The following decisions are deferred until the relevant module is written. They are recorded here so they are not forgotten and so the eventual decision (in a future ADR or commit) can reference the context.

**OQ-1 — `nmea0183` path-vs-version dependency.** The `talker` crate currently depends on `nmea0183` via `{ path = "../nmea0183" }`. When `nmea0183` is published to crates.io (per ADR-001), this should become `{ path = "../nmea0183", version = "0.1" }` so that downstream consumers building against published versions resolve cleanly while in-workspace builds continue to use the local source. Defer until publication is imminent.

**OQ-2 — `toml` 1.x vs 0.8 API. — Resolved (v2.0).** `core::profile` was implemented against `toml = "1"` with no friction: `Profile::load` parses to a `toml::Value` to inspect the schema version before full deserialization, then `toml::from_str` / `toml::to_string_pretty` handle the round trip. The 1.x API surface was sufficient; the fallback to `"0.8"` was not needed. The workspace stays on `toml = "1"`.

**OQ-3 — `nmea0183` serde feature activation in `talker`. — Resolved (v2.0).** `core::profile` uses a **`talker`-side representation**: an NMEA message is stored as `PayloadConfig::Nmea { talker: String, sentence_type: String, fields: Vec<String> }` — plain strings, not `nmea0183` types — and converted to a `nmea0183::NmeaSentence` only at compile time. The profile schema is therefore decoupled from the library's struct shapes, and the `talker` dependency on `nmea0183` does **not** enable the `serde` feature (`nmea0183 = { path = "../nmea0183" }`). The `serde` feature on `nmea0183` itself still exists and is still verified to compile, for the benefit of other potential consumers.

**OQ-4 — `nmea0183` library MSRV policy.** ADR-008 sets the workspace MSRV to current stable Rust. The `nmea0183` library, intended for crates.io publication, may benefit from a looser MSRV to accommodate cautious downstream users. The policy (e.g., N-6 months of stable releases) and the mechanism (per-crate `rust-version` override) are deferred to a future ADR when publication approaches. See ADR-008 for context.

New open questions should be added here as they arise during implementation.
