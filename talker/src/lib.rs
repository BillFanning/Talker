//! `talker` — send byte-oriented data over serial and network connections.
//!
//! All application logic lives in [`core`]; [`cli`] and [`gui`] are thin
//! interface layers over it. The binary (`src/main.rs`) is a small shim that
//! dispatches to one of those layers.

pub mod cli;
pub mod core;
pub mod gui;
