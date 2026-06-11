//! Integration tests: profile load/save behavior (spec §70, §159, §16.3).
//! Black-box — public API only.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use listener::config::{templates, Profile, RetentionConfig};
use listener::core::{ChannelKind, ChannelState};
use listener::runtime::Listener;

/// A unique temp path per call. Tests in one binary run concurrently, so the
/// path must not collide — a wall-clock stamp is unsafe (Windows clock resolution
/// is coarse enough for two parallel tests to share a nanosecond). A monotonic
/// counter plus the pid guarantees uniqueness.
fn temp_profile_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("listener-it-profile-{pid}-{n}.toml"))
}

#[test]
fn profile_round_trips_through_a_toml_file() {
    let mut profile = Profile::new("integration workspace");
    profile.channels = vec![
        templates::udp_template(),
        templates::tcp_listener_template(),
    ];

    let path = temp_profile_path();
    profile.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();
    assert_eq!(loaded, profile);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn loading_a_profile_does_not_start_channels() {
    // §70: loaded channels restore in Stopped state — load never starts a
    // channel or begins recording. There is no runtime state to restore
    // (message numbers, connections): the schema has no place for it (§159).
    let mut profile = Profile::new("workspace");
    profile.channels = vec![
        templates::udp_template(),
        templates::tcp_listener_template(),
    ];

    let path = temp_profile_path();
    profile.save(&path).unwrap();
    let loaded = Profile::load(&path).unwrap();

    let mut listener = Listener::with_default_capacities();
    for config in loaded.channels {
        let id = listener.add_channel(config);
        assert_eq!(listener.state(id), Some(ChannelState::Stopped));
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn profiles_cannot_contain_tcp_connection_channels() {
    // §16.3: TCP Connection channels are runtime-only and never persisted.
    let mut connection = templates::tcp_listener_template();
    connection.kind = ChannelKind::TcpConnection;
    let mut profile = Profile::new("workspace");
    profile.channels = vec![connection];

    let results = profile.validate();
    assert!(results[0].1.is_err());
}

#[test]
fn unbounded_retention_is_rejected() {
    // §80: a channel that sets no retention limit is invalid.
    let mut channel = templates::udp_template();
    channel.retention = RetentionConfig::default(); // all None
    let mut profile = Profile::new("workspace");
    profile.channels = vec![channel];

    assert!(profile.validate()[0].1.is_err());
}
