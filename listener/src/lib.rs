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

pub fn run() -> Result<()> {
    println!("listener: not yet implemented");
    Ok(())
}
