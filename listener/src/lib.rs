pub mod cli;
pub mod config;
pub mod core;
pub mod diagnostics;
pub mod display;
pub mod extract;
pub mod gui;
pub mod record;
pub mod retention;
pub mod runtime;
pub mod transport;

use anyhow::Result;

/// Binary entry point (§3): parse arguments, then dispatch to the graphical
/// interface or the headless CLI runner. Headless is the default whenever a source
/// is given (or `--cli`); a bare invocation (no source, no flag) opens the GUI, so
/// a double-clicked executable shows a window. The branch happens before the async
/// runtime is built, since the GUI owns its own runtime (ADR-008).
///
/// **Shared funnel (two-binary invariant — see `src/main.rs`).** The `listener.exe`
/// launcher calls this; the flash-free `listener-gui.exe` calls [`gui::run`]
/// directly. Process-wide startup that *both* binaries need belongs HERE (or in
/// [`gui::run`] for GUI-only startup), never inline in a `main`, so the two thin
/// launchers cannot drift.
pub fn run() -> Result<()> {
    let cli = cli::parse();
    if cli.wants_gui() {
        match gui::run() {
            Ok(()) => Ok(()),
            // A bare launch on a headless box can't open a window — guide toward
            // headless mode instead of surfacing a cryptic windowing error.
            Err(e) if cli.is_bare_launch() => Err(anyhow::anyhow!(
                "could not open the graphical interface ({e}); for headless use pass \
                 a source (--udp/--tcp/--serial/--profile), or run with --cli"
            )),
            Err(e) => Err(e),
        }
    } else {
        cli::run(cli)
    }
}
