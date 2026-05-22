use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Args as ClapArgs;

use crate::core::{
    channel::Interface,
    logging,
    profile::{self, Profile},
    scheduler::Schedule,
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
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> anyhow::Result<()> {
    if args.list_profiles {
        return list_profiles();
    }

    let profile = load_profile(&args)?;

    // Logging must be initialized before any tracing calls below.
    let _log = logging::init(&profile.logging, None).context("initializing logging")?;

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
        let schedule = Schedule::compile(&channel.messages)
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
    let mut handles = Vec::new();
    for (i, interface, schedule) in prepared {
        let running = Arc::clone(&running);
        handles.push(std::thread::spawn(move || {
            run_channel(i, interface, schedule, &running);
        }));
    }
    tracing::info!("{} channel(s) started — Ctrl+C to stop", handles.len());

    for handle in handles {
        let _ = handle.join();
    }
    tracing::info!("stopped");
    Ok(())
}

/// Send loop for one channel: fire each entry, wait its interval, repeat,
/// until `running` is cleared by the Ctrl+C handler.
fn run_channel(
    index: usize,
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    running: &AtomicBool,
) {
    while running.load(Ordering::SeqCst) {
        let entry = schedule.next_entry();
        if let Err(e) = interface.send(&entry.payload) {
            tracing::warn!("channel {index} send failed: {e:#}");
        }
        // Sleep in 50 ms slices so Ctrl+C is responsive even on long intervals.
        let deadline = Instant::now() + entry.interval;
        while running.load(Ordering::SeqCst) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
            Some(std::path::Path::new("/etc/talker/foo.toml"))
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
    fn load_profile_errors_when_neither_flag_given() {
        let args = Args {
            profile: None,
            profile_path: None,
            list_profiles: false,
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
        };
        let err = load_profile(&args).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }
}
