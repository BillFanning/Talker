use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args as ClapArgs;

use crate::core::{
    channel::Interface,
    logging::{self, FileLogConfig, Rotation},
    profile::{self, Profile},
    scheduler::{Schedule, Tick},
};

// ── Clap argument struct ──────────────────────────────────────────────────────

/// CLI arguments (flattened into the top-level command in `main.rs`).
#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Profile name to load from the default profile directory.
    #[arg(long, conflicts_with = "profile_path", value_name = "NAME")]
    pub profile: Option<String>,

    /// Path to a profile TOML file (overrides --profile).
    #[arg(long, conflicts_with = "profile", value_name = "FILE")]
    pub profile_path: Option<PathBuf>,

    /// List available profiles in the default directory and exit.
    #[arg(long)]
    pub list_profiles: bool,

    /// Echo each message sent to stdout, prefixed with its channel index.
    #[arg(long)]
    pub echo: bool,

    /// Suppress log output to stdout.
    #[arg(long)]
    pub quiet: bool,

    /// Write logs to a file. Given without a path, a default location is used.
    ///
    /// `None` = flag absent; `Some(None)` = flag given bare; `Some(Some(p))` =
    /// flag given with path `p`.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    pub log_file: Option<Option<PathBuf>>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> anyhow::Result<()> {
    if args.list_profiles {
        return list_profiles();
    }

    let profile = load_profile(&args)?;

    // Apply CLI overrides to the profile's logging configuration, then install
    // logging before any tracing call below.
    let mut logging_config = profile.logging.clone();
    if args.quiet {
        logging_config.stdout = false;
    }
    if let Some(log_file) = &args.log_file {
        logging_config.file = Some(resolve_log_file(log_file.as_deref())?);
    }
    let _log = logging::init(&logging_config, None).context("initializing logging")?;

    tracing::info!("profile {:?} loaded", profile.name);
    anyhow::ensure!(!profile.channels.is_empty(), "profile has no channels");

    // Open every channel's interface and compile its schedule up front, so a
    // failure aborts cleanly before any talker thread is spawned.
    let mut prepared: Vec<(usize, Box<dyn Interface>, Schedule)> = Vec::new();
    for (i, channel) in profile.channels.into_iter().enumerate() {
        let interface = channel
            .interface
            .open()
            .with_context(|| format!("opening channel {i}"))?;
        let schedule = Schedule::compile(&channel.messages, Instant::now())
            .with_context(|| format!("compiling channel {i} schedule"))?;
        prepared.push((i, interface, schedule));
    }

    // Shared stop flag written by the Ctrl+C handler.
    let running = Arc::new(AtomicBool::new(true));
    {
        let stop = Arc::clone(&running);
        ctrlc::set_handler(move || stop.store(false, Ordering::SeqCst))
            .context("installing Ctrl+C handler")?;
    }

    // One talker thread per channel — all run in parallel.
    let echo = args.echo;
    let mut handles = Vec::new();
    for (i, interface, schedule) in prepared {
        let running = Arc::clone(&running);
        handles.push(std::thread::spawn(move || {
            run_channel(i, interface, schedule, &running, echo);
        }));
    }
    tracing::info!("{} channel(s) started — Ctrl+C to stop", handles.len());

    for handle in handles {
        let _ = handle.join();
    }
    tracing::info!("stopped");
    Ok(())
}

/// Send loop for one channel: poll the schedule, send whatever is due, and
/// wait, until `running` is cleared by the Ctrl+C handler.
///
/// With `echo` set, each successful send is mirrored to stdout as hex, tagged
/// with the channel index so multi-channel output can be filtered.
fn run_channel(
    index: usize,
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    running: &AtomicBool,
    echo: bool,
) {
    while running.load(Ordering::SeqCst) {
        match schedule.poll(Instant::now()) {
            Tick::Send { payload, .. } => match interface.send(&payload) {
                Ok(()) => {
                    if echo {
                        println!("ch{index}: {}", hex_string(&payload));
                    }
                }
                Err(e) => tracing::warn!("channel {index} send failed: {e:#}"),
            },
            // Sleep in <=50 ms slices so Ctrl+C stays responsive on long waits.
            Tick::Wait(until) => {
                let remaining = until.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(50)));
            }
            Tick::Idle => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Format bytes as space-separated uppercase hex pairs.
fn hex_string(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a `--log-file` argument into a [`FileLogConfig`].
///
/// `None` (a bare `--log-file`) selects the platform default directory.
/// Otherwise the path is split into a directory and a filename prefix.
fn resolve_log_file(path: Option<&Path>) -> anyhow::Result<FileLogConfig> {
    let Some(path) = path else {
        let dir = logging::default_log_dir()
            .context("cannot determine a default log directory on this platform")?;
        return Ok(FileLogConfig::new(dir));
    };
    let directory = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let prefix = match path.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => "talker.log".to_string(),
    };
    Ok(FileLogConfig {
        directory,
        prefix,
        rotation: Rotation::default(),
    })
}

fn load_profile(args: &Args) -> anyhow::Result<Profile> {
    let path = if let Some(p) = &args.profile_path {
        p.clone()
    } else if let Some(name) = &args.profile {
        let dir = profile::default_dir().context("cannot determine profile directory")?;
        dir.join(format!("{name}.toml"))
    } else {
        anyhow::bail!("specify --profile <name> or --profile-path <path>");
    };
    Profile::load(&path)
}

fn list_profiles() -> anyhow::Result<()> {
    let dir = match profile::default_dir() {
        Some(d) => d,
        None => {
            println!("Cannot determine profile directory on this platform.");
            return Ok(());
        }
    };

    if !dir.exists() {
        println!(
            "No profiles found (directory {} does not exist).",
            dir.display()
        );
        return Ok(());
    }

    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .collect();

    if names.is_empty() {
        println!("No profiles found in {}.", dir.display());
    } else {
        names.sort();
        for name in &names {
            println!("{name}");
        }
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Args, clap::Error> {
        use clap::Parser;

        #[derive(Parser)]
        struct Cmd {
            #[command(flatten)]
            args: Args,
        }

        Cmd::try_parse_from(argv).map(|c| c.args)
    }

    #[test]
    fn parse_profile_name() {
        let a = parse(&["talker", "--profile", "my-profile"]).unwrap();
        assert_eq!(a.profile.as_deref(), Some("my-profile"));
        assert!(a.profile_path.is_none());
    }

    #[test]
    fn parse_profile_path() {
        let a = parse(&["talker", "--profile-path", "/etc/talker/foo.toml"]).unwrap();
        assert_eq!(
            a.profile_path.as_deref(),
            Some(Path::new("/etc/talker/foo.toml"))
        );
        assert!(a.profile.is_none());
    }

    #[test]
    fn parse_list_profiles() {
        let a = parse(&["talker", "--list-profiles"]).unwrap();
        assert!(a.list_profiles);
    }

    #[test]
    fn profile_and_profile_path_conflict() {
        let result = parse(&[
            "talker",
            "--profile",
            "foo",
            "--profile-path",
            "/bar/baz.toml",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_echo_and_quiet_flags() {
        let a = parse(&["talker", "--profile", "p", "--echo", "--quiet"]).unwrap();
        assert!(a.echo);
        assert!(a.quiet);
        let b = parse(&["talker", "--profile", "p"]).unwrap();
        assert!(!b.echo);
        assert!(!b.quiet);
    }

    #[test]
    fn parse_log_file_with_path() {
        let a = parse(&["talker", "--profile", "p", "--log-file", "/var/log/run.log"]).unwrap();
        assert_eq!(a.log_file, Some(Some(PathBuf::from("/var/log/run.log"))));
    }

    #[test]
    fn parse_log_file_bare_yields_some_none() {
        let a = parse(&["talker", "--profile", "p", "--log-file"]).unwrap();
        assert_eq!(a.log_file, Some(None));
    }

    #[test]
    fn parse_log_file_absent_is_none() {
        let a = parse(&["talker", "--profile", "p"]).unwrap();
        assert!(a.log_file.is_none());
    }

    #[test]
    fn load_profile_errors_when_neither_flag_given() {
        let args = Args {
            profile: None,
            profile_path: None,
            list_profiles: false,
            echo: false,
            quiet: false,
            log_file: None,
        };
        let err = load_profile(&args).unwrap_err();
        assert!(err.to_string().contains("--profile"));
    }

    #[test]
    fn load_profile_errors_on_missing_file() {
        let args = Args {
            profile: None,
            profile_path: Some(PathBuf::from("/no/such/file.toml")),
            list_profiles: false,
            echo: false,
            quiet: false,
            log_file: None,
        };
        let err = load_profile(&args).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }

    #[test]
    fn hex_string_formats_uppercase_spaced() {
        assert_eq!(hex_string(&[0xDE, 0xAD, 0xBE, 0xEF]), "DE AD BE EF");
        assert_eq!(hex_string(&[0x01]), "01");
        assert_eq!(hex_string(&[]), "");
    }

    #[test]
    fn resolve_log_file_splits_directory_and_prefix() {
        let cfg = resolve_log_file(Some(Path::new("/var/log/talker/run.log"))).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("/var/log/talker"));
        assert_eq!(cfg.prefix, "run.log");
    }

    #[test]
    fn resolve_log_file_bare_filename_uses_current_dir() {
        let cfg = resolve_log_file(Some(Path::new("run.log"))).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("."));
        assert_eq!(cfg.prefix, "run.log");
    }

    #[test]
    fn resolve_log_file_none_uses_default_location() {
        // On platforms without a local-data directory this returns an error;
        // otherwise it resolves to the default log directory.
        if let Ok(cfg) = resolve_log_file(None) {
            assert!(cfg.directory.ends_with("logs"));
        }
    }
}
