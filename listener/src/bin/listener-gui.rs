//! Flash-free GUI entry point → `listener-gui.exe` (spec §3).
//!
//! Built as a Windows **"windows" subsystem** app, so a double-click opens the
//! window without the OS allocating a console — no console flash. It does nothing
//! but launch the GUI. The sibling `listener` binary (src/main.rs) stays a
//! **console** app so the headless CLI behaves from a terminal (real stdout,
//! working Ctrl-C, the shell waits for it). A single binary can't be both: a
//! console app flashes a console on double-click, and a windows-subsystem app
//! can't do interactive terminal CLI — hence two entry points.
//!
//! ── TWO-BINARY INVARIANT (full note in src/main.rs) ──
//! Keep this launcher DUMB. The whole reason the two-binary drift risk is low is
//! that this file is ~3 lines — keep it that way. It must stay a thin wrapper that
//! calls into the shared `listener` library; any startup step (logging, console
//! handling, panic hooks, …) belongs in the library (`listener::gui::run`), never
//! inline here, so this binary and `listener.exe` cannot diverge.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    // One call only. Shared GUI startup (console detach, logging, window options)
    // lives in `gui::run`, so this binary and `listener.exe`'s GUI path are
    // identical. (See the two-binary invariant in src/main.rs.)
    listener::gui::run()
}
