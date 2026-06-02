# TODO — Listener

Implementation reminders for the `listener` crate. Not architectural decisions
(those go in [`ADR.md`](ADR.md) or the spec). Workspace-level tasks live in
[`talker/docs/TODO.md`](../../talker/docs/TODO.md).

Cross off items as they are completed. Add new ones inline as they come up.

---

## Implementation order (spec §147)

Build in this order — do **not** start with the GUI. Each unit is a `src/` module
(single crate, per ADR-004 / spec §127):

- [x] `core/` module — common types, Message/metadata model, `ChannelId`, state enums, errors
- [x] `extract/` module
- [x] extraction tests
- [x] `runtime/` skeleton
- [x] queue / backpressure tests (the Transport→Extractor `blocking_send` path, §99)
- [x] UDP transport
- [x] serial transport (dedicated OS thread + bounded read timeout for cancellation, §111)
- [x] TCP listener/connection transport (runtime mints `ChannelId` per accepted connection)
- [x] recording
- [x] display
- [ ] GUI / CLI

## Scaffolding

- [x] Stand up `lib.rs` declaring the §127 modules + thin `main.rs` shim (talker ADR-014 shape).

_OQ-L1 (single crate vs. multi-crate) is resolved — see ADR-004. Revisit a crate split only when an external consumer or compile-time concern justifies it._

## Tests required (spec §150–§152)

- [ ] Channel operation, TCP connections, message processing, display, metadata/timing, recording, profiles, NMEA.
- [ ] Backpressure: prove the extractor never stalls on a slow consumer and that reader stall is reportable as transport-specific loss (§99, §101).

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
