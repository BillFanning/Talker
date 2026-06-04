pub mod cli;
pub mod config;
pub mod core;
pub mod decode;
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
/// interface (`--gui`) or the headless CLI runner. The branch happens before the
/// async runtime is built, since the GUI owns its own runtime (ADR-008).
pub fn run() -> Result<()> {
    let cli = cli::parse();
    if cli.gui {
        gui::run()
    } else {
        cli::run(cli)
    }
}
