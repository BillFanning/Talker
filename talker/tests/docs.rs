//! Workspace documentation checks.
//!
//! Lives under `talker/tests/` for want of a workspace-root crate to host it,
//! but its subject is every crate's `docs/` folder — it walks up from this
//! manifest to the workspace root.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("talker/ has a parent")
        .to_path_buf()
}

/// Every `.md` under a `docs/` folder, plus the working agreement itself.
fn documents(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let agreement = root.join("AGENTS.md");
    if agreement.is_file() {
        found.push(agreement);
    }
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let docs = entry.path().join("docs");
        let Ok(files) = fs::read_dir(&docs) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_some_and(|e| e == "md") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Every `.rs` file in the workspace, concatenated. Crude on purpose: the
/// question is only whether an identifier still appears anywhere in the source.
fn all_source(root: &Path) -> String {
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                if let Ok(text) = fs::read_to_string(&path) {
                    out.push_str(&text);
                    out.push('\n');
                }
            }
        }
    }
    let mut out = String::new();
    walk(root, &mut out);
    out
}

/// Backticked snake_case identifiers with at least two underscores — long
/// enough to be a function or constant rather than a prose word in backticks.
fn cited_identifiers(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for span in text.split('`').skip(1).step_by(2) {
        let underscores = span.matches('_').count();
        let plausible = underscores >= 2
            && !span.is_empty()
            && span
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && span.starts_with(|c: char| c.is_ascii_lowercase())
            && !span.ends_with('_');
        if plausible {
            found.push(span.to_owned());
        }
    }
    found
}

/// Identifiers a document names deliberately *because* they are gone.
///
/// Keep this short and reasoned. An entry is a claim that the surrounding prose
/// reads correctly with the identifier absent — usually a correction recording
/// what a decision used to rest on.
const REMOVED_ON_PURPOSE: &[(&str, &str)] = &[(
    "min_active_interval",
    "talker TODO names the old symbol beside the one that replaced it \
     (`active_cadence`); the sentence is about the rename",
)];

/// A document may not cite an identifier the source no longer contains.
///
/// This is the failure that keeps recurring: a rename or a deletion lands, and
/// a specification or ADR goes on asserting a guarantee by the name of a test
/// that no longer exists. Two consecutive external reviews found instances by
/// hand; this found four more on its first run, which is the argument for
/// having it rather than trusting another read-through.
///
/// Deliberately crude. It proves an identifier appears *somewhere* in the
/// source, not that the surrounding claim is true — the cheap half of the
/// problem, and the half that rots without anyone touching the document.
#[test]
fn documents_do_not_cite_identifiers_the_source_has_dropped() {
    let root = workspace_root();
    let source = all_source(&root);
    let docs = documents(&root);
    assert!(
        docs.len() > 5,
        "found only {} documents — the walk is not reaching docs/",
        docs.len()
    );

    let mut dangling: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for doc in &docs {
        let Ok(text) = fs::read_to_string(doc) else {
            continue;
        };
        let relative = doc
            .strip_prefix(&root)
            .unwrap_or(doc)
            .to_string_lossy()
            .replace('\\', "/");
        for ident in cited_identifiers(&text) {
            if source.contains(&ident) {
                continue;
            }
            if REMOVED_ON_PURPOSE.iter().any(|(name, _)| *name == ident) {
                continue;
            }
            let entry = dangling.entry(relative.clone()).or_default();
            if !entry.contains(&ident) {
                entry.push(ident);
            }
        }
    }

    assert!(
        dangling.is_empty(),
        "documents cite identifiers that no longer exist in any source file.\n\
         Fix the reference, or — if the document names it precisely because it \
         was removed — add it to REMOVED_ON_PURPOSE with a reason.\n{dangling:#?}"
    );
}
