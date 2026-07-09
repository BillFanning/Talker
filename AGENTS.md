# AGENTS.md

Working agreement and codebase guide for the **wiredata** workspace. Applies to all
three crates (`talker`, `nmea0183`, `listener`) and to **any** contributor — human or
coding agent (Claude Code, Codex, or otherwise). This is the tool-neutral source of
truth; tool-specific files (e.g. `CLAUDE.md`) should import it rather than duplicate it.

---

## 1. Authority & precedence

When guidance conflicts, follow this order (earlier wins):

1. An explicit instruction from the user in the current task.
2. The relevant crate's **spec** (`<crate>/docs/*_specification.md` or `*-spec-*.md`).
3. The relevant crate's **ADR** (`<crate>/docs/ADR.md`).
4. This file (cross-cutting working rules).

Specs and ADRs are authoritative. **Do not invent architecture or scope** — implement
from the spec. If the spec is silent or seems wrong, ask or record a decision (below);
do not guess and proceed.

## 2. Where things live

Each crate owns a `docs/` folder:

| Crate | Spec | Decisions | Tasks |
|-------|------|-----------|-------|
| `talker` | [talker/docs/talker_specification.md](talker/docs/talker_specification.md) | [talker/docs/ADR.md](talker/docs/ADR.md) | [talker/docs/TODO.md](talker/docs/TODO.md) |
| `nmea0183` | [nmea0183/docs/nmea0183_specification.md](nmea0183/docs/nmea0183_specification.md) | [nmea0183/docs/ADR.md](nmea0183/docs/ADR.md) | [nmea0183/docs/TODO.md](nmea0183/docs/TODO.md) |
| `listener` | [listener/docs/listener_specification.md](listener/docs/listener_specification.md) | [listener/docs/ADR.md](listener/docs/ADR.md) | [listener/docs/TODO.md](listener/docs/TODO.md) |

- `wiredata-ui` (the shared GUI-chrome crate) has **no** `docs/` folder: it is internal
  and small by design. Its decisions live in the app ADR series — talker ADR-016 and
  listener ADR-019 — and any chrome change that alters both apps' look should reference
  them.
- Record any non-trivial design choice as a **new ADR entry** in the owning crate's
  `ADR.md` (talker and nmea0183 share one ADR number series; listener has its own).
- Track concrete implementation reminders in the owning crate's `TODO.md`.
- **Deferred / out-of-scope features** are normative in each spec (e.g. listener spec
  Appendix A). Do not implement them without a spec amendment.

### Document versioning

These rules apply to every versioned document across the workspace (specs, ADRs,
TODOs, and any other versioned file):

- A document's version number lives **only at the top** of the document (its header) —
  never repeated anywhere else in the body.
- **Don't put version numbers in filenames.** Keep the version in the header only, so a
  bump is a one-line edit with no rename or reference churn. Our specs follow this:
  `talker_specification.md`, `nmea0183_specification.md`, `listener_specification.md`.
  (If a filename ever does carry a version, the filename and the in-document version
  must stay in sync — rename on every change.)
- **Never change a version number without checking with the user first.** Get explicit
  approval before any bump.
- **Every version change ships with a revision note** at the top of the document,
  summarizing what changed.

## 3. Scope & workflow discipline

- **Test-first** for core logic: write the unit/integration tests before or alongside
  the code, not after.
- Keep changes minimal and on-task; don't refactor unrelated code in passing.
- Before finishing, run the checks in §4 (build, test, clippy, fmt).
- Commits: imperative subject, explain *why* in the body; group related changes.
- Never commit secrets; profiles and logs may contain real device data.

---

## 4. Build and development commands

```powershell
# Build / test the entire workspace
cargo build
cargo build --release
cargo test

# Run a crate binary
cargo run -p talker -- --gui
cargo run -p talker -- --profile <name>
cargo run -p listener

# Test a single crate / a specific test
cargo test -p nmea0183
cargo test -p talker
cargo test -p nmea0183 checksum::tests::xor_basic

# Lint and format (run before finishing)
cargo clippy -- -D warnings
cargo fmt
cargo fmt -- --check

# Verify nmea0183 optional serde feature compiles cleanly
cargo build -p nmea0183
cargo build -p nmea0183 --features serde
```

MSRV is **1.95** (current stable). Run `rustup update stable` if the build rejects your
toolchain.

---

## 5. Architecture

### Workspace layout

Four crates in a Cargo workspace:

- **`nmea0183/`** — library crate; no dependency on `talker` or `listener`; intended for
  independent crates.io publication. Handles NMEA 0183 sentence construction, parsing,
  checksum, talker IDs, proprietary sentences (`$PRDID`, `$PASHR`, arbitrary `$P`), and
  AIS sentences (`!AIVDM`/`!AIVDO` with 6-bit payload armoring). Used by `talker`;
  `listener` **dropped** its `nmea0183` dependency in the v2.0 stream-only strip
  (ADR-010 — no decoding), so today only `talker` consumes it.
- **`talker/`** — library plus a thin binary (ADR-014). Sends/schedules data out over
  serial and network interfaces. All application logic lives in `core/`; `cli/` and
  `gui/` are thin interface layers that contain no business logic. `main.rs` only
  dispatches; `lib.rs` exports the modules so `core`'s API is unit-testable and the
  default dead-code lint stays active.
- **`listener/`** — receives byte-oriented data **streams** from serial and network
  sources (the inbound counterpart to `talker`). v2.0 is stream-only (ADR-010): it
  displays/records the verbatim byte stream — no decoding, no `nmea0183` dependency.
  Single crate with modular internals (listener ADR-004 / spec §127); `lib.rs` + thin
  `main.rs`, same shape as `talker`.
- **`wiredata-ui/`** — internal (`publish = false`) shared GUI **chrome** for the two
  apps (talker ADR-016 / listener ADR-019): the bundled font stack + assets, the named
  color palette (`LIGHT`/`DARK`), the base widget visuals for both themes, shared style
  tweaks, and pure formatting helpers. Depends **only on `egui`** — never on `talker`,
  `listener`, `eframe`, or any runtime crate. A piece belongs here only if it is purely
  presentational, identical across both apps, and egui-only; app-specific widgets,
  layouts, and view-models stay in the apps.

```
talker/src/
├── main.rs          # thin shim: dispatches to CLI or GUI based on args
├── lib.rs           # pub mod cli; pub mod core; pub mod gui;
├── cli/             # clap argument parsing; calls into core
├── gui/             # egui/eframe UI; calls into core
└── core/
    ├── channel/     # serial, UDP unicast/broadcast/multicast, TCP interfaces
    ├── message/     # payload formats, encoding, code pages, byte markers, timestamps, checksums
    ├── scheduler/   # priority-queue schedule: per-message send intervals
    ├── profile/     # TOML load/save, schema v2 (clean break — no migration)
    └── logging/     # tracing-subscriber setup; GUI status pane layer
```

### Threading / runtime model

The two binaries deliberately reach **opposite** runtime conclusions, each fitting its
I/O shape (see talker ADR-002 vs listener ADR-001):

- **`talker` — no async runtime.** Three thread roles, `crossbeam-channel` as the only
  IPC:

  | Thread | Owns |
  |--------|------|
  | UI thread | egui/eframe event loop; never blocks, never does I/O |
  | Talker thread (one per channel) | channel interface handle; scheduler; send loop |
  | Logger thread | receives events via a crossbeam channel; writes to file and/or stdout |

  Each talker thread has a dedicated pair of crossbeam channels with the UI (commands
  down, status up). Interface handles are never shared across threads.

- **`listener` — Tokio hybrid.** Tokio owns orchestration (commands, cancellation,
  bounded queues, fan-out, async network I/O, shutdown); continuous blocking serial
  receive loops run on dedicated OS threads that hand data to the async side through
  bounded `tokio::sync::mpsc` channels. See listener spec §97.

### Key design rules (cross-crate)

- `nmea0183` must not import application-level crates (`anyhow`, `eframe`, `clap`,
  `tokio`, etc.). It stays a pure, publishable library.
- `wiredata-ui` imports only `egui`. It must never depend on `talker`, `listener`,
  `eframe`, or runtime crates — chrome only (talker ADR-016 / listener ADR-019).
- UI threads never perform I/O and never block.
- `cli/` and `gui/` are thin layers; business logic lives in `core/` (or the equivalent
  internal modules).
- `core::channel` (talker) manages a **collection** of channels from day one — there is
  no single-channel shortcut to refactor away later.
- Prefer passing shared config by value through channels over `Mutex` where avoidable.

### Error handling

- `nmea0183` uses `thiserror` → typed `NmeaError` enum. Add `#[non_exhaustive]` before
  crates.io publication.
- `talker` and `listener` use `anyhow` → wrap with `.context()` for user-facing
  messages. Never panic in production code paths.
- The `NmeaError`→`anyhow::Error` conversion happens at the crate boundary via `?`.

### Profiles (talker)

- Format: TOML via `serde` + `toml = "1"` (OQ-2 resolved — the 1.x API is sufficient).
- Every profile struct field gets `#[serde(default)]`, so additive schema changes need
  no migration code.
- Header field `version: u32` — current schema is **2** (`CURRENT_VERSION` in
  `core::profile`). `Profile::load` refuses any profile whose version differs: a newer
  version is unsupported, and v1 is rejected with a "recreate the profile" error. This
  is a deliberate clean break — there is no `migration` module (ADR-013 update).
- An NMEA payload is stored as plain strings (`PayloadConfig::Nmea { talker,
  sentence_type, fields }`), not `nmea0183` types; the `nmea0183` dependency does not
  enable the `serde` feature (OQ-3 resolved).
- Profiles are CLI/GUI compatible. GUI-only state (window geometry, last active profile)
  is stored separately by `eframe`'s built-in persistence.
- Profile enums use `#[non_exhaustive]`.

### NMEA 0183 (`nmea0183` crate)

- NMEA XOR checksum is implemented inline (trivial byte fold) — no `crc` crate dependency.
- `talker` uses the `crc` crate for general checksums (CRC-8, CRC-16/CCITT,
  CRC-16/MODBUS, CRC-32, XOR).
- `TalkerId` and sentence type enums have a `Custom(String)` variant for non-standard IDs.
- `ProprietarySentence` has named variants (`Prdid`, `Pashr`) and a `Raw` variant.
  `$PRDID` does **not** include a checksum by convention. `$PASHR` field 10 (GNSS
  quality) is exposed as raw `u8` — Trimble and Novatel define the values differently.
- AIS sentences (`!AIVDM`/`!AIVDO`) use `!` as the start character and the `AI` talker
  ID. `AisSentence` builds and parses them; `armor`/`unarmor` handle the 6-bit ASCII
  payload encoding. AIS is a `nmea0183` library capability only — it is **not** a
  `talker` message format (spec §5.1 lists five formats, none of them AIS).
- Serde derives on all public types are gated behind
  `#[cfg_attr(feature = "serde", derive(...))]`.

### Message formats (`talker` `core::message`)

A channel owns one or more messages; each message has a payload format, an optional
prepended timestamp, and an optional appended checksum. `PayloadConfig` variants
(spec §5.1):

- `RawHex` — arbitrary bytes as a hex string (spaces and hyphens stripped).
- `Utf8` / `Ascii` — text. `Ascii` carries a `CodePage` (CP437, Windows-1252, Mac OS
  Roman, ISO-8859-1 — hand-written tables, ADR-015).
- `Utf16` — text with a `ByteOrder` (`BigEndian` default | `LittleEndian`) and an
  optional BOM.
- `Nmea` — a sentence built through the `nmea0183` crate.

`Utf8`/`Ascii` text may carry non-printable bytes as inline `‹XX›` markers (U+2039, two
hex digits, U+203A — spec §5.3); `core::message::marker` splits marker-aware text and
`compile()` expands the markers into raw bytes. `compile()` produces the static wire
bytes; the timestamp, if any, is rendered per send.

### Logging

`tracing` facade + `tracing-subscriber`. The GUI status pane is a
`tracing_subscriber::Layer` that forwards events to the UI thread via `crossbeam-channel`.
File log rotation uses `tracing-appender` (a workspace dependency).

---

## 6. Testing conventions

- Unit tests: `#[cfg(test)]` block at the bottom of the file under test.
- Integration tests: `<crate>/tests/` (e.g. `talker/tests/`, `nmea0183/tests/`).
- All `core` (and equivalent internal) modules need unit tests covering normal, edge,
  and error cases.
- `nmea0183` tests cover every sentence type: parsing, construction, and checksum.
- For `listener`, write extraction and queue/backpressure tests before adding transports
  (spec §147).
