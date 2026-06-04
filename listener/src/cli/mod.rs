//! CLI presentation layer (spec §3).
//!
//! A thin layer over the runtime [`Listener`](crate::runtime::Listener): it
//! turns arguments (or a profile) into channel configs, starts them, prints the
//! `RuntimeEvent` stream, and shuts down gracefully on Ctrl-C. It contains no
//! business logic — channel construction lives in `runtime`/`config` (§3, §128).

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::config::{templates, ChannelConfig, DecoderConfig, InterfaceConfig, Profile};
use crate::core::RuntimeEvent;
use crate::decode::NmeaValidationMode;
use crate::runtime::Listener;

/// Receive, decode, and inspect byte-oriented data from serial and network sources.
#[derive(Parser, Debug)]
#[command(name = "listener", version, about)]
pub struct Cli {
    /// Load a workspace profile (TOML) and start all its valid channels.
    #[arg(long, value_name = "PATH")]
    profile: Option<PathBuf>,

    /// Quick start: receive UDP datagrams on this port (binds 0.0.0.0).
    #[arg(long, value_name = "PORT")]
    udp: Option<u16>,

    /// Quick start: accept TCP clients on this port (binds 0.0.0.0).
    #[arg(long, value_name = "PORT")]
    tcp: Option<u16>,

    /// Quick start: open this serial port (e.g. COM3 or /dev/ttyUSB0).
    #[arg(long, value_name = "PORT")]
    serial: Option<String>,

    /// Serial baud rate (used with --serial).
    #[arg(long, default_value_t = 9600)]
    baud: u32,

    /// Decode received Messages as NMEA0183 (for the quick-start channels).
    #[arg(long)]
    nmea: bool,
}

/// CLI entry point: parse args, build a Tokio runtime, run the event loop.
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    crate::diagnostics::init_logging(); // §114; non-fatal if already installed (§117)
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    runtime.block_on(run_cli(cli))
}

async fn run_cli(cli: Cli) -> Result<()> {
    let configs = build_channel_configs(&cli)?;

    let mut listener = Listener::with_default_capacities();
    let mut events = listener
        .take_events()
        .expect("the event stream is available exactly once");

    let mut started = 0usize;
    for config in configs {
        let name = config.name.as_str().to_string();
        let id = listener.add_channel(config);
        match listener.start(id).await {
            Ok(()) => {
                println!("started \"{name}\" [{id}]");
                started += 1;
            }
            Err(err) => eprintln!("could not start \"{name}\": {err}"),
        }
    }
    if started == 0 {
        bail!("no channels started");
    }

    println!("listening on {started} channel(s) — press Ctrl-C to stop");
    // Drive auto-reconnect (§162): the orchestrator has no background loop, so the
    // app ticks it. Channels without reconnect enabled are unaffected.
    let mut reconnect = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = reconnect.tick() => listener.reconnect_tick().await,
            maybe = events.recv() => match maybe {
                Some(event) => println!("{}", format_event(&event)),
                None => break,
            },
        }
    }

    println!("stopping…");
    listener.shutdown().await;
    Ok(())
}

/// Build the channels to run from the profile or the quick-start flags. Per §71,
/// invalid channels in a profile are reported and skipped, not fatal.
fn build_channel_configs(cli: &Cli) -> Result<Vec<ChannelConfig>> {
    if let Some(path) = &cli.profile {
        let profile =
            Profile::load(path).with_context(|| format!("loading profile {}", path.display()))?;
        let mut valid = Vec::new();
        for (config, (name, result)) in profile.channels.iter().zip(profile.validate()) {
            match result {
                Ok(()) => valid.push(config.clone()),
                Err(errors) => eprintln!("skipping invalid channel \"{name}\": {errors:?}"),
            }
        }
        return Ok(valid);
    }

    let mut config = if let Some(port) = cli.udp {
        let mut config = templates::udp_template();
        if let InterfaceConfig::Udp(udp) = &mut config.interface {
            udp.port = port;
        }
        config
    } else if let Some(port) = cli.tcp {
        let mut config = templates::tcp_listener_template();
        if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
            tcp.port = port;
        }
        config
    } else if let Some(port) = &cli.serial {
        let mut config = templates::serial_template();
        if let InterfaceConfig::Serial(serial) = &mut config.interface {
            serial.port = port.clone();
            serial.baud_rate = cli.baud;
        }
        config
    } else {
        bail!("specify --profile, --udp, --tcp, or --serial (see --help)");
    };

    if cli.nmea {
        config.decoder = DecoderConfig::Nmea0183 {
            validation_mode: NmeaValidationMode::Standard,
        };
    }
    Ok(vec![config])
}

fn format_event(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::ChannelStarted(id) => format!("[{id}] started"),
        RuntimeEvent::ChannelStopped(id) => format!("[{id}] stopped"),
        RuntimeEvent::ChannelFaulted(id) => format!("[{id}] FAULTED"),
        RuntimeEvent::MessageReceived(id, number) => format!("[{id}] message #{number}"),
        RuntimeEvent::RecordingFaulted(id) => format!("[{id}] recording faulted"),
        RuntimeEvent::WarningRaised(id) => format!("[{id}] warning raised"),
        RuntimeEvent::ReceptionStalled(id, dur) => {
            format!("[{id}] reception stalled {} ms", dur.as_millis())
        }
        RuntimeEvent::TcpClientConnected(id) => format!("[{id}] TCP client connected"),
        RuntimeEvent::TcpClientDisconnected(id) => format!("[{id}] TCP client disconnected"),
        RuntimeEvent::ControlLinesChanged(id) => format!("[{id}] control lines changed"),
        RuntimeEvent::ChannelReconnecting(id, attempt) => {
            format!("[{id}] reconnecting (attempt {attempt})")
        }
        RuntimeEvent::ChannelReconnected(id) => format!("[{id}] reconnected"),
        RuntimeEvent::ChannelReconnectGaveUp(id) => format!("[{id}] reconnect gave up"),
        RuntimeEvent::DiskSpaceLow(id) => format!("[{id}] LOW DISK"),
        RuntimeEvent::RecordingStoppedLowDisk(id) => {
            format!("[{id}] recording stopped (low disk)")
        }
        RuntimeEvent::MatchTriggered(id, rule) => format!("[{id}] match rule {rule} fired"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cli() -> Cli {
        Cli {
            profile: None,
            udp: None,
            tcp: None,
            serial: None,
            baud: 9600,
            nmea: false,
        }
    }

    #[test]
    fn udp_quick_start_sets_the_port() {
        let cli = Cli {
            udp: Some(9000),
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        assert_eq!(configs.len(), 1);
        match &configs[0].interface {
            InterfaceConfig::Udp(udp) => assert_eq!(udp.port, 9000),
            other => panic!("expected UDP, got {other:?}"),
        }
    }

    #[test]
    fn serial_quick_start_sets_port_and_baud() {
        let cli = Cli {
            serial: Some("COM7".to_string()),
            baud: 4800,
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        match &configs[0].interface {
            InterfaceConfig::Serial(serial) => {
                assert_eq!(serial.port, "COM7");
                assert_eq!(serial.baud_rate, 4800);
            }
            other => panic!("expected serial, got {other:?}"),
        }
    }

    #[test]
    fn nmea_flag_selects_the_decoder() {
        let cli = Cli {
            udp: Some(9000),
            nmea: true,
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        assert!(matches!(configs[0].decoder, DecoderConfig::Nmea0183 { .. }));
    }

    #[test]
    fn no_source_is_an_error() {
        assert!(build_channel_configs(&base_cli()).is_err());
    }

    #[test]
    fn profile_loads_valid_channels_and_skips_invalid() {
        let mut path = std::env::temp_dir();
        path.push(format!("listener-cli-{}.toml", uuid::Uuid::new_v4()));

        let mut profile = Profile::new("test workspace");
        let mut bad = templates::udp_template();
        bad.retention = crate::config::RetentionConfig::default(); // all-None → invalid (§80)
        profile.channels = vec![templates::udp_template(), bad];
        profile.save(&path).unwrap();

        let cli = Cli {
            profile: Some(path.clone()),
            ..base_cli()
        };
        let configs = build_channel_configs(&cli).unwrap();
        // The valid channel loads; the unbounded-retention one is skipped.
        assert_eq!(configs.len(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
