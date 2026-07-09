//! Pure string/number formatting helpers for the GUI readouts — now shared
//! with talker via `wiredata-ui` (ADR-019); re-exported here so call sites
//! are unchanged.

pub(crate) use wiredata_ui::format::human_bytes;
