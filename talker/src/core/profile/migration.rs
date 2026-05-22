//! Migration functions from older profile versions to the current version.
//!
//! # How to add a new schema version
//!
//! 1. Bump `CURRENT_VERSION` in `super` (mod.rs).
//! 2. Write a `migrate_vN_to_vNp1(doc: &mut toml::Value)` function here that
//!    surgically edits the document in place (rename keys, add defaults, etc.).
//! 3. Add a branch to `migrate()` for `from_version == N`.
//!
//! Each function receives the full profile document as a `toml::Value::Table`
//! and should leave it in the shape expected by version N+1. The caller in
//! `mod.rs` stamps the new `version` field after all steps complete.

/// Upgrade `doc` from `from_version` to [`super::CURRENT_VERSION`] in place.
///
/// At schema version 1 there are no prior versions to migrate from, so this
/// is a no-op. Add sequential upgrade steps here when the schema is bumped.
pub fn migrate(_doc: &mut toml::Value, from_version: u32) -> anyhow::Result<()> {
    // Example for a future v2 bump:
    // if from_version < 2 { migrate_v1_to_v2(_doc)?; }
    anyhow::ensure!(
        from_version < super::CURRENT_VERSION,
        "migrate() called with from_version ({from_version}) >= CURRENT_VERSION; nothing to do"
    );
    Ok(())
}
