use clap::Parser;

use talker::{cli, core, gui};

#[derive(Parser)]
#[command(
    name = "talker",
    about = "Send NMEA 0183 and binary data over serial and network connections"
)]
struct TopArgs {
    /// Launch the GUI.
    #[arg(long)]
    gui: bool,

    #[command(flatten)]
    cli: cli::Args,
}

fn main() {
    let args = TopArgs::parse();

    if args.gui {
        let initial_profile = if let Some(p) = args.cli.profile_path {
            Some(p)
        } else if let Some(name) = args.cli.profile {
            core::profile::default_dir().map(|d| d.join(format!("{name}.toml")))
        } else {
            None
        };
        if let Err(e) = gui::run(initial_profile) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    if let Err(e) = cli::run(args.cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
