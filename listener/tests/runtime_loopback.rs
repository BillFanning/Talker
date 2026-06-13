//! Integration tests: loopback UDP/TCP channels driven through the runtime
//! orchestrator (spec §153–§155). Black-box — public API only. v2.0 is stream-
//! only: there are no Messages, so tests assert on the verbatim stream scrollback
//! and byte-based liveness via on-demand snapshots (ADR-010).

use std::time::Duration;

use listener::config::InterfaceConfig;
use listener::core::{ChannelId, ChannelState, RuntimeEvent};
use listener::runtime::{ChannelSnapshot, Listener};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;

/// Bind an ephemeral loopback UDP port, then release it for the channel to claim.
fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Poll a running channel's snapshot until `pred` is satisfied, or time out.
/// Replaces the v1 `MessageReceived`-event wait — liveness is byte-based now.
async fn await_snapshot(
    listener: &Listener,
    id: ChannelId,
    pred: impl Fn(&ChannelSnapshot) -> bool,
) -> ChannelSnapshot {
    let wait = async {
        loop {
            if let Some(s) = listener.snapshot(id).await {
                if pred(&s) {
                    return s;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for the snapshot condition")
}

/// Fetch a running channel's full stream scrollback verbatim via the incremental
/// stream path (§87, ADR-009) — the bytes are no longer bundled into the snapshot.
async fn stream_bytes(listener: &Listener, id: ChannelId) -> Vec<u8> {
    listener
        .stream_delta(id, 0)
        .await
        .map(|d| d.bytes.to_vec())
        .unwrap_or_default()
}

/// Await the next `TcpClientConnected` event, with a timeout.
async fn next_connection(events: &mut Receiver<RuntimeEvent>) -> ChannelId {
    let wait = async {
        loop {
            match events.recv().await.expect("event stream closed") {
                RuntimeEvent::TcpClientConnected(id) => return id,
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a connection")
}

async fn stop(listener: &mut Listener, id: ChannelId) {
    tokio::time::timeout(Duration::from_secs(5), listener.stop(id))
        .await
        .expect("stop hung")
        .expect("stop failed");
}

/// Await the next `MatchTriggered` event, with a timeout.
async fn next_match(events: &mut Receiver<RuntimeEvent>) -> ChannelId {
    let wait = async {
        loop {
            match events.recv().await.expect("event stream closed") {
                RuntimeEvent::MatchTriggered(id, _rule) => return id,
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a match")
}

#[tokio::test]
async fn udp_channel_receives_datagrams_and_stops_cleanly() {
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();
    assert_eq!(listener.state(id), Some(ChannelState::Running));

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();

    // The datagrams land in the stream scrollback, concatenated verbatim (§18).
    let snap = await_snapshot(&listener, id, |s| s.stream_end_offset >= 10).await;
    assert_eq!(snap.activity.total_bytes, 10);
    assert_eq!(stream_bytes(&listener, id).await, b"alphabravo");

    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn udp_channel_match_rule_fires_an_action_and_is_observable() {
    use listener::config::{MatchAction, MatchCondition, MatchRule};
    use listener::diagnostics::DiagnosticSeverity;

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    // A byte-pattern rule that raises a Notify when "ALARM" appears (§50.2, §165).
    config.match_rules = vec![MatchRule {
        name: "alarm".to_string(),
        condition: MatchCondition::BytePattern {
            pattern: b"ALARM".to_vec(),
        },
        actions: vec![MatchAction::Notify {
            severity: DiagnosticSeverity::Warning,
        }],
        enabled: true,
    }];

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // A non-matching datagram: received, but no rule fires.
    client.send_to(b"quiet", ("127.0.0.1", port)).await.unwrap();
    // A matching datagram: the rule fires (push half — MatchTriggered event).
    client
        .send_to(b"ALARM now", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_match(&mut events).await, id);

    // Pull half: the firing is in the snapshot, anchored at the matching chunk's
    // stream byte offset (5, after "quiet"), and Notify left a warning diagnostic.
    let snap = listener
        .snapshot(id)
        .await
        .expect("a running channel snapshot");
    assert_eq!(snap.matches.len(), 1);
    assert_eq!(snap.matches[0].byte_offset, Some(5));
    assert_eq!(snap.diagnostics.warnings.len(), 1);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn tcp_listener_accepts_a_client_and_receives_data() {
    let port = free_tcp_port();
    let mut config = listener::config::templates::tcp_listener_template();
    if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
        tcp.bind_address = "127.0.0.1".to_string();
        tcp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    // A fresh connection channel is minted, distinct from the listener (§16.4).
    let conn_id = next_connection(&mut events).await;
    assert_ne!(conn_id, id);

    client.write_all(b"sentence\n").await.unwrap();

    // Stopping the listener terminates its connections (§13). (Per-connection
    // snapshots are deferred, so we assert the lifecycle here.)
    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn snapshot_exposes_the_verbatim_stream_of_a_running_channel() {
    // Observability surface (§137, ADR-006): a live snapshot reveals the verbatim
    // received bytes — including a bad-checksum NMEA sentence carried as plain bytes.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"$GPGLL,4916.45,N,12311.12,W*00", ("127.0.0.1", port))
        .await
        .unwrap();
    // A deliberately wrong checksum is "bad data is still data" — carried verbatim.
    client
        .send_to(b"$GPHDT,274.07,T*FF", ("127.0.0.1", port))
        .await
        .unwrap();

    let expected: &[u8] = b"$GPGLL,4916.45,N,12311.12,W*00$GPHDT,274.07,T*FF";
    let snapshot = await_snapshot(&listener, id, |s| {
        s.stream_end_offset >= expected.len() as u64
    })
    .await;
    assert_eq!(snapshot.channel_id, id);
    assert_eq!(stream_bytes(&listener, id).await, expected);

    // A stopped channel has no live pipeline to snapshot.
    stop(&mut listener, id).await;
    assert!(listener.snapshot(id).await.is_none());
}

#[tokio::test]
async fn rotation_writes_a_named_period_file_through_the_orchestrator() {
    // §163: a rotating Raw recording writes a period file named
    // <channel>_<period>.raw into the destination directory, driven through the
    // orchestrator. Boundary-crossing across periods is unit-tested in
    // record::file_rotation with crafted timestamps; here we prove wiring + naming.
    use listener::config::RecordingConfig;
    use listener::record::{FileRotationPolicy, OverwritePolicy, RecordingMode};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "listener-rot-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    config.name = listener::core::ChannelName::new("gpsfeed");
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.recording = RecordingConfig {
        mode: RecordingMode::Raw,
        destination: Some(dir.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::Hourly,
        disk_guard: None,
    };

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"$GPGGA,test", ("127.0.0.1", port))
        .await
        .unwrap();
    let _ = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 11).await;

    stop(&mut listener, id).await; // finalizes + flushes the recording

    // Exactly one rotated file, named gpsfeed_<period>.raw, holding the datagram.
    let mut raws: Vec<_> = std::fs::read_dir(&dir)
        .expect("rotation directory exists")
        .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap()))
        .filter(|n| n.starts_with("gpsfeed_") && n.ends_with(".raw"))
        .collect();
    raws.sort();
    assert_eq!(raws.len(), 1, "one period file, got {raws:?}");
    assert_eq!(std::fs::read(dir.join(&raws[0])).unwrap(), b"$GPGGA,test");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn snapshot_surfaces_channel_liveness() {
    // §166: a running channel's snapshot reports byte-based liveness — throughput
    // and total bytes registered, and the last-data time set once data arrives.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"hello-liveness", ("127.0.0.1", port))
        .await
        .unwrap();

    let snap = await_snapshot(&listener, id, |s| s.activity.total_bytes > 0).await;
    assert!(snap.activity.last_data_at.is_some(), "data has arrived");
    assert!(snap.activity.bytes_per_sec > 0.0, "throughput registered");
    assert_eq!(snap.activity.total_bytes, 14);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn pausing_a_display_view_freezes_only_the_stream_display() {
    // §50/§58: pausing the (default) Display View freezes the stream scrollback;
    // reception, the byte counter, and recording continue.
    let port = free_udp_port();
    let mut config = listener::config::templates::udp_template(); // Raw + Hex → two views
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let views = listener.display_views(id);
    assert_eq!(views.len(), 2);
    // Pausing the default (first) view freezes the shared scrollback (§50.1 removed;
    // the scrollback honors the default view's pause).
    listener.pause_display(id, views[0]).unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();

    // The byte counter advances even though the scrollback stays frozen/empty.
    let snap = await_snapshot(&listener, id, |s| s.activity.total_bytes >= 10).await;
    assert!(snap.display_views[0].paused);
    assert_eq!(snap.stream_end_offset, 0); // scrollback frozen while paused
    assert!(stream_bytes(&listener, id).await.is_empty());
    assert_eq!(snap.activity.total_bytes, 10);

    // Resuming accumulates only new data — no backfill of what was missed.
    listener.resume_display(id, views[0]).unwrap();
    client
        .send_to(b"charlie", ("127.0.0.1", port))
        .await
        .unwrap();
    let snap = await_snapshot(&listener, id, |s| s.stream_end_offset > 0).await;
    assert_eq!(stream_bytes(&listener, id).await, b"charlie"); // only post-resume data
    assert_eq!(snap.activity.total_bytes, 17);

    stop(&mut listener, id).await;
}
