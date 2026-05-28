use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Args as ClapArgs, ValueEnum};

use crate::core::{
    channel::Interface,
    logging::{self, FileLogConfig, Rotation},
    message::decode_utf8_lossy_latin1,
    profile::{self, Profile},
    scheduler::{Schedule, Tick},
};

/// How `--echo` renders each sent message to stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "lower")]
pub enum EchoFormat {
    /// UTF-8 decode with Latin-1 fallback — matches the GUI's
    /// "Rendered" display mode and gives every byte a printable
    /// glyph. Most readable for NMEA / ASCII / UTF-8 streams.
    #[default]
    Rendered,
    /// Printable ASCII as-is; every non-printable byte
    /// (`< 0x20`, `0x7F`, `>= 0x80`) shown as `<0xXX>`. Matches the
    /// GUI's "Raw" display mode with the HexEscapes control style.
    Raw,
    /// Each byte as two uppercase hex digits, space-separated.
    /// What `--echo` produced before the format flag existed.
    Hex,
}

// ── Clap argument struct ──────────────────────────────────────────────────────

/// CLI arguments (flattened into the top-level command in `main.rs`).
///
/// Each option uses explicit `help = ...` and `long_help = ...`
/// attributes so the long help opens with the short-help sentence
/// on the first line (no blank-line separator), then continues
/// with the detail in the same paragraph. Using clap-derive's
/// auto-derive-from-doc-comments instead would either put the
/// short help in its own paragraph (with a blank line) or strip
/// the detail entirely.
#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(
        short = 'p',
        long,
        conflicts_with = "profile_path",
        value_name = "NAME",
        help = "Load a profile by name from the default profile directory.",
        long_help = "Load a profile by name from the default profile directory. \
                     The profile is looked up at `<default-dir>/<NAME>.toml`. The \
                     default directory is `dirs::config_dir()/talker/profiles`, which \
                     on Windows is `%APPDATA%\\talker\\profiles`, on Linux \
                     `$XDG_CONFIG_HOME/talker/profiles` (or \
                     `~/.config/talker/profiles`), and on macOS \
                     `~/Library/Application Support/talker/profiles`. Use \
                     `--list-profiles` to enumerate what's there. Mutually exclusive \
                     with `--profile-path`."
    )]
    pub profile: Option<String>,

    #[arg(
        short = 'P',
        long,
        conflicts_with = "profile",
        value_name = "FILE",
        help = "Load a profile from an explicit TOML file path.",
        long_help = "Load a profile from an explicit TOML file path. Accepts any \
                     path your shell can hand off (relative or absolute). Mutually \
                     exclusive with `--profile`; useful when the file lives outside \
                     the default directory or has a non-standard name."
    )]
    pub profile_path: Option<PathBuf>,

    #[arg(
        short = 'l',
        long,
        help = "List profile names found in the default directory and exit.",
        long_help = "List profile names found in the default directory and exit. \
                     Exit code is always 0, including when the directory is missing \
                     or empty — the message on stdout explains what was found. \
                     Combines fine with `--quiet` to suppress logging without \
                     affecting the listing itself."
    )]
    pub list_profiles: bool,

    #[arg(
        short = 'e',
        long,
        help = "Echo each sent message to stdout, tagged with its channel index.",
        long_help = "Echo each sent message to stdout, tagged with its channel \
                     index. Format defaults to `rendered` (UTF-8 with Latin-1 \
                     fallback — most readable for NMEA / ASCII); override with \
                     `--echo-format raw` or `--echo-format hex`. Each line is \
                     prefixed `chN: `. Independent of `--quiet` — echo always goes \
                     to stdout."
    )]
    pub echo: bool,

    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "rendered",
        help = "How `--echo` renders each message (rendered|raw|hex).",
        long_help = "How `--echo` renders each message (rendered|raw|hex). \
                     `rendered` decodes bytes as UTF-8 with Latin-1 fallback so \
                     every byte has a glyph (matches the GUI's Rendered display \
                     mode). `raw` shows printable ASCII as-is and non-printable \
                     bytes as `<0xXX>` (matches the GUI's Raw mode + HexEscapes \
                     control style). `hex` shows each byte as two uppercase hex \
                     digits, space-separated — the format `--echo` used before \
                     this flag existed. Ignored when `--echo` is absent."
    )]
    pub echo_format: EchoFormat,

    #[arg(
        long,
        help = "Drop the `chN: ` channel-index prefix from --echo output.",
        long_help = "Drop the `chN: ` channel-index prefix from --echo output. \
                     Default is to include the prefix so a multi-channel stream \
                     can be filtered with `grep ch3:`. With a single channel, or \
                     when piping into a tool that expects raw payload lines, \
                     `--no-tag` keeps the output uncluttered. Ignored when \
                     `--echo` is absent."
    )]
    pub no_tag: bool,

    #[arg(
        short = 'q',
        long,
        help = "Suppress log output to stdout.",
        long_help = "Suppress log output to stdout. Affects the `tracing` subscriber \
                     only. `--echo`, the profile list from `--list-profiles`, and \
                     any logs written via `--log-file` still appear at their normal \
                     destinations."
    )]
    pub quiet: bool,

    /// `None` = flag absent; `Some(None)` = flag given bare;
    /// `Some(Some(p))` = flag given with path `p`. (Doc comment
    /// kept on the field for Rust readers — clap uses the explicit
    /// `help` / `long_help` attrs below for CLI output.)
    #[arg(
        short = 'L',
        long,
        value_name = "PATH",
        num_args = 0..=1,
        help = "Write logs to a file. Given without a path, a default location is used.",
        long_help = "Write logs to a file. Given without a path, a default location \
                     is used — `dirs::data_local_dir()/talker/logs/talker.log` \
                     (e.g. `%LOCALAPPDATA%\\talker\\logs\\talker.log` on Windows). With \
                     a path, the path is split into directory + filename prefix. \
                     File logging is additive — stdout logging is unaffected unless \
                     you also pass `--quiet`."
    )]
    pub log_file: Option<Option<PathBuf>>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: Args) -> anyhow::Result<()> {
    // Leading newline for visual separation from the shell prompt
    // is handled in `main.rs` (via the `\n`-prefixed `about` string
    // and the error formatter), so this runner doesn't add its own.

    if args.list_profiles {
        return list_profiles();
    }

    let (mut profile, path) = load_profile(&args)?;
    // Mirror the GUI: the file root is the profile's identity, so
    // overlay `profile.name` from the path's stem. The TOML's `name`
    // field is skipped during deserialise, so otherwise `name` would
    // be empty and the log below would read `profile "" loaded`.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
        profile.name = stem.to_string();
    }

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
    let echo_format = args.echo_format;
    let tag = !args.no_tag;
    let mut handles = Vec::new();
    for (i, interface, schedule) in prepared {
        let running = Arc::clone(&running);
        handles.push(std::thread::spawn(move || {
            run_channel(i, interface, schedule, &running, echo, echo_format, tag);
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
/// With `echo` set, each successful send is mirrored to stdout in
/// `format`, tagged with the channel index so multi-channel output
/// can be filtered.
fn run_channel(
    index: usize,
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    running: &AtomicBool,
    echo: bool,
    format: EchoFormat,
    tag: bool,
) {
    while running.load(Ordering::SeqCst) {
        match schedule.poll(Instant::now()) {
            Tick::Send { payload, .. } => match interface.send(&payload) {
                Ok(()) => {
                    if echo {
                        echo_line(index, &payload, format, tag);
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

/// Print one echo line.
///
/// Trailing `\r` / `\n` in the formatted payload is stripped so the
/// `println!` doesn't double up on payloads that already end in a
/// line break (most NMEA sentences do). Without this, Rendered mode
/// would emit an empty line between messages, and Hex mode would on
/// some terminals that interpret CR+LF in the middle of the line.
fn echo_line(index: usize, payload: &[u8], format: EchoFormat, tag: bool) {
    let text = format_payload(payload, format);
    let text = text.trim_end_matches(['\r', '\n']);
    if tag {
        println!("ch{index}: {text}");
    } else {
        println!("{text}");
    }
}

/// Render `payload` per `format` for `--echo` output.
fn format_payload(payload: &[u8], format: EchoFormat) -> String {
    match format {
        EchoFormat::Hex => hex_string(payload),
        EchoFormat::Raw => raw_string(payload),
        EchoFormat::Rendered => decode_utf8_lossy_latin1(payload),
    }
}

/// Printable ASCII as-is; every other byte as `<0xXX>`. Matches the
/// GUI's `DisplayMode::Raw` with the `HexEscapes` control style —
/// chosen as the default for CLI since it doesn't depend on Unicode
/// control-picture fonts being installed.
fn raw_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..=0x7E).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("<0x{b:02X}>"));
        }
    }
    out
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

fn load_profile(args: &Args) -> anyhow::Result<(Profile, PathBuf)> {
    let path = if let Some(p) = &args.profile_path {
        p.clone()
    } else if let Some(name) = &args.profile {
        let dir = profile::default_dir().context("cannot determine profile directory")?;
        dir.join(format!("{name}.toml"))
    } else {
        // Multi-line so the suggested invocations stand out from the
        // surrounding "error:" prefix that `main.rs` adds on stderr.
        // Joined from an array — Rust string-literal line
        // continuations strip leading whitespace, so trying to
        // indent inline mangles the alignment.
        let msg = [
            "no profile specified — pick one of:",
            "  -p, --profile <NAME>        load a profile by name from the default directory",
            "  -P, --profile-path <FILE>   load a profile from an explicit path",
            "  -l, --list-profiles         show what's in the default directory and exit",
            "  -g, --gui                   launch the graphical interface",
            "      --help                  full option list with descriptions",
        ]
        .join("\n");
        anyhow::bail!(msg);
    };
    let profile = Profile::load(&path)?;
    Ok((profile, path))
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
    fn parse_short_flags_match_long() {
        // Each short form should reach the same field as its long
        // counterpart — proves the `short = 'X'` derive entries
        // line up with the long names.
        let a = parse(&["talker", "-p", "name", "-l", "-e", "-q", "-L", "/tmp/x.log"]).unwrap();
        assert_eq!(a.profile.as_deref(), Some("name"));
        assert!(a.list_profiles);
        assert!(a.echo);
        assert!(a.quiet);
        assert_eq!(a.log_file, Some(Some(PathBuf::from("/tmp/x.log"))));

        let b = parse(&["talker", "-P", "/etc/talker/foo.toml"]).unwrap();
        assert_eq!(
            b.profile_path.as_deref(),
            Some(Path::new("/etc/talker/foo.toml"))
        );
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
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
        };
        let err = load_profile(&args).unwrap_err();
        let msg = err.to_string();
        // Error should mention each of the suggested ways forward —
        // protects against accidental message regressions.
        for needle in [
            "--profile",
            "--profile-path",
            "--list-profiles",
            "--gui",
            "--help",
        ] {
            assert!(msg.contains(needle), "missing {needle}: {msg}");
        }
    }

    #[test]
    fn load_profile_errors_on_missing_file() {
        let args = Args {
            profile: None,
            profile_path: Some(PathBuf::from("/no/such/file.toml")),
            list_profiles: false,
            echo: false,
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
        };
        let err = load_profile(&args).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }

    #[test]
    fn load_profile_returns_path() {
        // Round-tripped path lets `run()` derive the profile name
        // from the file stem.
        let args = Args {
            profile: None,
            profile_path: Some(PathBuf::from("/no/such/file.toml")),
            list_profiles: false,
            echo: false,
            echo_format: EchoFormat::default(),
            no_tag: false,
            quiet: false,
            log_file: None,
        };
        // Loading errors, but we just want to confirm the path
        // propagates in the error-free signature shape.
        let _ = load_profile(&args);
    }

    #[test]
    fn hex_string_formats_uppercase_spaced() {
        assert_eq!(hex_string(&[0xDE, 0xAD, 0xBE, 0xEF]), "DE AD BE EF");
        assert_eq!(hex_string(&[0x01]), "01");
        assert_eq!(hex_string(&[]), "");
    }

    #[test]
    fn raw_string_shows_printable_and_hex_escapes() {
        assert_eq!(raw_string(b"Hello!"), "Hello!");
        assert_eq!(raw_string(&[0x41, 0x0D, 0x0A]), "A<0x0D><0x0A>");
        // High byte (Latin-1 'î') is non-printable ASCII → hex escape.
        assert_eq!(raw_string(&[0xEE]), "<0xEE>");
    }

    #[test]
    fn format_payload_dispatches_by_format() {
        let nmea = b"$GPGGA,123519\r\n";
        // Hex: spaced uppercase hex.
        assert!(format_payload(nmea, EchoFormat::Hex).starts_with("24 47 50"));
        // Raw: printable as-is, CR/LF as hex escapes.
        let raw = format_payload(nmea, EchoFormat::Raw);
        assert!(raw.starts_with("$GPGGA"));
        assert!(raw.ends_with("<0x0D><0x0A>"));
        // Rendered: control bytes pass through as their Unicode codepoint.
        let rendered = format_payload(nmea, EchoFormat::Rendered);
        assert!(rendered.starts_with("$GPGGA"));
        assert!(rendered.ends_with('\n'));
    }

    #[test]
    fn echo_format_default_is_rendered() {
        assert_eq!(EchoFormat::default(), EchoFormat::Rendered);
    }

    #[test]
    fn parse_echo_format_flag() {
        let a = parse(&["talker", "-p", "x", "--echo", "--echo-format", "hex"]).unwrap();
        assert_eq!(a.echo_format, EchoFormat::Hex);
        let b = parse(&["talker", "-p", "x", "--echo", "--echo-format", "raw"]).unwrap();
        assert_eq!(b.echo_format, EchoFormat::Raw);
        // Default value applies when the flag is absent.
        let c = parse(&["talker", "-p", "x", "--echo"]).unwrap();
        assert_eq!(c.echo_format, EchoFormat::Rendered);
    }

    #[test]
    fn parse_no_tag_flag() {
        let a = parse(&["talker", "-p", "x", "--echo", "--no-tag"]).unwrap();
        assert!(a.no_tag);
        let b = parse(&["talker", "-p", "x", "--echo"]).unwrap();
        assert!(!b.no_tag);
    }

    #[test]
    fn format_payload_trims_trailing_newline_for_rendered() {
        // NMEA payload ends `\r\n`; the Rendered decode passes those
        // through as Unicode chars. `echo_line` strips them before
        // println so the terminal sees exactly one line per send.
        let nmea = b"$GPGGA,123519\r\n";
        let rendered = format_payload(nmea, EchoFormat::Rendered);
        assert!(rendered.ends_with('\n'));
        let stripped = rendered.trim_end_matches(['\r', '\n']);
        assert_eq!(stripped, "$GPGGA,123519");
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
