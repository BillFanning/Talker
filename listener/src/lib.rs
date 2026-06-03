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

/// Binary entry point: run the CLI (§3). The GUI front-end is a separate path
/// (not yet implemented).
pub fn run() -> Result<()> {
    cli::run()
}
