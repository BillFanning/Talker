//! Shared GUI **chrome** for the wiredata apps (talker ADR-016 / listener
//! ADR-019): the bundled font stack, the severity/status color palette, the
//! base widget style (both themes), and small pure formatting helpers.
//!
//! Scope is deliberately narrow — the pieces that make the two apps *look and
//! feel* like one product. App-specific widgets, layouts, and view-models stay
//! in each app. This crate depends only on `egui`; it must never depend on
//! `talker`, `listener`, `eframe`, or any runtime crate.

pub mod fonts;
pub mod format;
pub mod glyphs;
pub mod palette;
pub mod repaint;
pub mod style;
