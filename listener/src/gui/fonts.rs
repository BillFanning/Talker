//! Font stack — moved verbatim to the shared `wiredata-ui` crate (ADR-019) so
//! talker installs the identical faces. This module re-exports it under the
//! old paths, so gui call sites are unchanged. The bundled files and their
//! rationale live in `wiredata-ui/assets/fonts/README.md`.

pub(crate) use wiredata_ui::fonts::MonoFont;
pub(super) use wiredata_ui::fonts::{bold, install_fonts};
