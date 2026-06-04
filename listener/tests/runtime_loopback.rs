//! Integration tests: loopback UDP/TCP channels driven through the runtime
//! orchestrator (spec §153–§155). Black-box — public API only.

use std::time::Duration;

use listener::config::{templates, DecoderConfig, ExtractionConfig, InterfaceConfig};
use listener::core::{ChannelId, ChannelState, IntegrityScope, IntegrityStatus, RuntimeEvent};
use listener::decode::NmeaValidationMode;
use listener::runtime::Listener;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::Receiver;

/// Build a well-formed NMEA sentence from its body (the text between `$` and
/// `*`), computing the correct XOR checksum (§35) so the decoder reports Valid.
fn nmea(body: &str) -> Vec<u8> {
    let checksum = body.bytes().fold(0u8, |acc, b| acc ^ b);
    format!("${body}*{checksum:02X}").into_bytes()
}

/// Like [`nmea`] but with a deliberately wrong checksum, so the decoder reports
/// Invalid — the "bad data is still data" path (§37).
fn nmea_bad_checksum(body: &str) -> Vec<u8> {
    let wrong = body.bytes().fold(0u8, |acc, b| acc ^ b) ^ 0xFF;
    format!("${body}*{wrong:02X}").into_bytes()
}

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

/// Await the next `MessageReceived` event, ignoring lifecycle events; times out
/// rather than hanging the suite on a bug.
async fn next_message(events: &mut Receiver<RuntimeEvent>) -> (ChannelId, u64) {
    let wait = async {
        loop {
            match events.recv().await.expect("event stream closed") {
                RuntimeEvent::MessageReceived(id, number) => return (id, number),
                _ => continue,
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("timed out waiting for a message")
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
    let mut config = templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();
    assert_eq!(listener.state(id), Some(ChannelState::Running));

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();

    // Each datagram is one Message; numbering starts at 1 (§15, §24).
    assert_eq!(next_message(&mut events).await, (id, 1));
    assert_eq!(next_message(&mut events).await, (id, 2));

    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn udp_channel_match_rule_fires_an_action_and_is_observable() {
    use listener::config::{MatchAction, MatchCondition, MatchRule};
    use listener::diagnostics::DiagnosticSeverity;

    let port = free_udp_port();
    let mut config = templates::udp_template();
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
    // A non-matching datagram: a Message, but no rule fires.
    client.send_to(b"quiet", ("127.0.0.1", port)).await.unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));
    // A matching datagram: the rule fires (push half — MatchTriggered event).
    client
        .send_to(b"ALARM now", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_match(&mut events).await, id);

    // Pull half: the firing is in the snapshot, on message #2, and Notify left a
    // warning diagnostic — reception is unaffected.
    let snap = listener
        .snapshot(id)
        .await
        .expect("a running channel snapshot");
    assert_eq!(snap.matches.len(), 1);
    assert_eq!(snap.matches[0].message_number, Some(2));
    assert_eq!(snap.diagnostics.warnings.len(), 1);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn tcp_listener_accepts_a_client_and_receives_a_message() {
    let port = free_tcp_port();
    let mut config = templates::tcp_listener_template();
    if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
        tcp.bind_address = "127.0.0.1".to_string();
        tcp.port = port;
    }
    // Accepted connections inherit the listener's extraction (§16.2); use an LF
    // delimiter so a line becomes a Message.
    config.extraction = ExtractionConfig::Delimiter {
        delimiter: vec![b'\n'],
        include_delimiter: false,
    };

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
    assert_eq!(next_message(&mut events).await, (conn_id, 1));

    // Stopping the listener terminates its connections (§13).
    stop(&mut listener, id).await;
    assert_eq!(listener.state(id), Some(ChannelState::Stopped));
}

#[tokio::test]
async fn snapshot_exposes_decoded_messages_of_a_running_channel() {
    // #4 observability surface: while the channel runs, an on-demand snapshot
    // reveals the retained Messages *and* their decoder annotations — the decode
    // readout the event stream alone can't carry (§137, ADR-006).
    let port = free_udp_port();
    let mut config = templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.decoder = DecoderConfig::Nmea0183 {
        validation_mode: NmeaValidationMode::Standard,
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // A valid GLL sentence (one datagram → one Message, §15).
    client
        .send_to(b"$GPGLL,4916.45,N,12311.12,W*00", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));

    let snapshot = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    assert_eq!(snapshot.channel_id, id);
    assert_eq!(snapshot.next_message_number, 2);
    assert_eq!(snapshot.retained.len(), 1);
    // The decoder annotation is observable live — this is the readout #2 deferred.
    let decoded = &snapshot.retained[0];
    assert_eq!(decoded.message.number, 1);
    assert_eq!(
        decoded
            .protocol
            .as_ref()
            .expect("decoder ran")
            .message_type
            .as_deref(),
        Some("GLL")
    );
    // Every Display View (the template configures Raw + Hex) accumulated it.
    assert_eq!(snapshot.display_views.len(), 2);
    assert!(snapshot.display_views.iter().all(|v| v.messages.len() == 1));

    // A stopped channel has no live pipeline to snapshot.
    stop(&mut listener, id).await;
    assert!(listener.snapshot(id).await.is_none());
}

#[tokio::test]
async fn rotation_writes_a_named_period_file_through_the_orchestrator() {
    // §163: a rotating Raw recording writes a period file named
    // <channel>_<period>.dat into the destination directory, driven through the
    // orchestrator. Boundary-crossing across periods is unit-tested in
    // record::file_rotation with crafted timestamps; here we prove wiring + naming.
    use listener::config::{RecordingConfig, Subsample};
    use listener::record::{FileRotationPolicy, OverwritePolicy, RecordingMode};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "listener-rot-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = templates::udp_template();
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
        subsample: Subsample::None,
        disk_guard: None,
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"$GPGGA,test", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));

    stop(&mut listener, id).await; // finalizes + flushes the recording

    // Exactly one rotated file, named gpsfeed_<period>.dat, holding the datagram.
    let mut dats: Vec<_> = std::fs::read_dir(&dir)
        .expect("rotation directory exists")
        .filter_map(|e| e.ok().map(|e| e.file_name().into_string().unwrap()))
        .filter(|n| n.starts_with("gpsfeed_") && n.ends_with(".dat"))
        .collect();
    dats.sort();
    assert_eq!(dats.len(), 1, "one period file, got {dats:?}");
    assert_eq!(std::fs::read(dir.join(&dats[0])).unwrap(), b"$GPGGA,test");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn subsampling_thins_a_view_without_renumbering_or_affecting_others() {
    // §164: a view's subsampler passes a subset of Messages with their original
    // (gapped, not renumbered) numbers, while retention and other views see every
    // Message — reception/numbering are unaffected.
    use listener::config::Subsample;

    let port = free_udp_port();
    let mut config = templates::udp_template(); // Raw + Hex views
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    // Subsample only the first (Raw) view: 1 of every 2 Messages.
    config.display.views[0].subsample = Subsample::EveryNth { n: 2 };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for n in 1..=4u8 {
        client
            .send_to(&[b'0' + n], ("127.0.0.1", port))
            .await
            .unwrap();
        assert_eq!(next_message(&mut events).await, (id, n as u64));
    }

    let snap = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    let nums = |v: &listener::runtime::DisplayViewSnapshot| -> Vec<u64> {
        v.messages.iter().map(|d| d.message.number).collect()
    };
    // Subsampled view: 1 of every 2, original Message Numbers (gapped, not renumbered).
    assert_eq!(nums(&snap.display_views[0]), vec![1, 3]);
    // The other view saw every Message.
    assert_eq!(nums(&snap.display_views[1]), vec![1, 2, 3, 4]);
    // Reception/retention/numbering unaffected.
    let retained: Vec<u64> = snap.retained.iter().map(|d| d.message.number).collect();
    assert_eq!(retained, vec![1, 2, 3, 4]);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn subsampled_data_recording_writes_decimated_message_bytes() {
    // §164/§53: a Raw recording with subsampling becomes a message-framed .ssdat
    // holding only the passed Messages' bytes (decimated), not byte-exact .dat.
    use listener::config::{RecordingConfig, Subsample};
    use listener::record::{FileRotationPolicy, OverwritePolicy, RecordingMode};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "listener-ssdat-{}-{}.ssdat",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let port = free_udp_port();
    let mut config = templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.recording = RecordingConfig {
        mode: RecordingMode::Raw,
        destination: Some(path.clone()),
        timestamp_enabled: false,
        overwrite_policy: OverwritePolicy::Overwrite,
        file_rotation: FileRotationPolicy::None,
        subsample: Subsample::EveryNth { n: 2 },
        disk_guard: None,
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for (n, payload) in [b"A", b"B", b"C", b"D"].into_iter().enumerate() {
        client.send_to(payload, ("127.0.0.1", port)).await.unwrap();
        assert_eq!(next_message(&mut events).await, (id, n as u64 + 1));
    }

    stop(&mut listener, id).await; // finalizes + flushes the recording

    // 1 of every 2 Messages: passes #1 ("A") and #3 ("C") → "AC", decimated.
    assert_eq!(std::fs::read(&path).unwrap(), b"AC");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn snapshot_surfaces_channel_liveness() {
    // §166: a running channel's snapshot reports liveness — throughput registered
    // and the last-data time set once data has arrived.
    let port = free_udp_port();
    let mut config = templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(b"hello-liveness", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));

    let snap = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    assert!(snap.activity.last_data_at.is_some(), "data has arrived");
    assert!(snap.activity.bytes_per_sec > 0.0, "throughput registered");
    assert!(snap.activity.messages_per_sec > 0.0);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn snapshot_exposes_message_metadata_and_nmea_integrity() {
    // §157/§160 acceptance: a live snapshot carries per-Message metadata (number,
    // byte count, arrival, reception duration) and NMEA integrity — and a
    // bad-checksum sentence is retained and annotated Invalid, never dropped (§37).
    let port = free_udp_port();
    let mut config = templates::udp_template();
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }
    config.decoder = DecoderConfig::Nmea0183 {
        validation_mode: NmeaValidationMode::Standard,
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let before = std::time::SystemTime::now();
    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let good = nmea("GPGLL,4916.45,N,12311.12,W");
    let bad = nmea_bad_checksum("GPHDT,274.07,T");
    client.send_to(&good, ("127.0.0.1", port)).await.unwrap();
    client.send_to(&bad, ("127.0.0.1", port)).await.unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));
    assert_eq!(next_message(&mut events).await, (id, 2));

    let snapshot = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    assert_eq!(snapshot.retained.len(), 2);

    // Message 1: valid checksum, full metadata.
    let m1 = &snapshot.retained[0];
    assert_eq!(m1.message.number, 1);
    assert_eq!(m1.message.metadata.total_byte_count, good.len());
    // One datagram is one chunk, so reception duration is zero (§105).
    assert_eq!(m1.message.metadata.reception_duration, Some(Duration::ZERO));
    // Arrival timestamp is sane: captured after we started sending.
    assert!(m1.message.metadata.arrival_timestamp.wall_clock >= before);
    let p1 = m1.protocol.as_ref().expect("decoder ran");
    assert_eq!(p1.message_type.as_deref(), Some("GLL"));
    let xor1 = &p1.integrity[0];
    assert_eq!(xor1.scope, IntegrityScope::Protocol);
    assert_eq!(xor1.status, IntegrityStatus::Valid);
    assert_eq!(xor1.algorithm.as_deref(), Some("NMEA XOR"));

    // Message 2: bad checksum — retained, annotated Invalid, byte-exact (§37).
    let m2 = &snapshot.retained[1];
    assert_eq!(m2.message.number, 2);
    assert_eq!(&*m2.message.bytes, bad.as_slice());
    let p2 = m2.protocol.as_ref().expect("decoder ran");
    assert_eq!(p2.message_type.as_deref(), Some("HDT"));
    assert_eq!(p2.integrity[0].status, IntegrityStatus::Invalid);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn pausing_a_display_view_freezes_only_that_view() {
    // §156 acceptance: pausing one Display View freezes only its accumulation;
    // the other view, retention, and numbering keep going — proven via snapshots.
    let port = free_udp_port();
    let mut config = templates::udp_template(); // Raw + Hex → two views
    if let InterfaceConfig::Udp(udp) = &mut config.interface {
        udp.bind_address = "127.0.0.1".to_string();
        udp.port = port;
    }

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let views = listener.display_views(id);
    assert_eq!(views.len(), 2);
    let (paused_id, live_id) = (views[0], views[1]);
    listener.pause_display(id, paused_id).unwrap();

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"alpha", ("127.0.0.1", port)).await.unwrap();
    client.send_to(b"bravo", ("127.0.0.1", port)).await.unwrap();
    assert_eq!(next_message(&mut events).await, (id, 1));
    assert_eq!(next_message(&mut events).await, (id, 2));

    let snap = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    let view = |vid| snap.display_views.iter().find(|v| v.id == vid).unwrap();
    // The paused view accumulated nothing; the live view got both.
    assert!(view(paused_id).paused);
    assert_eq!(view(paused_id).messages.len(), 0);
    assert!(!view(live_id).paused);
    assert_eq!(view(live_id).messages.len(), 2);
    // Reception, numbering, and retention were unaffected by the pause (§50).
    assert_eq!(snap.retained.len(), 2);
    assert_eq!(snap.next_message_number, 3);

    // Resuming accumulates only new Messages — no backfill of what was missed.
    listener.resume_display(id, paused_id).unwrap();
    client
        .send_to(b"charlie", ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(next_message(&mut events).await, (id, 3));
    let snap = listener
        .snapshot(id)
        .await
        .expect("running channel snapshots");
    let view = |vid| snap.display_views.iter().find(|v| v.id == vid).unwrap();
    assert_eq!(view(paused_id).messages.len(), 1); // only "charlie"
    assert_eq!(view(live_id).messages.len(), 3);

    stop(&mut listener, id).await;
}

#[tokio::test]
async fn tcp_connection_inherits_the_listener_decoder() {
    // §16.2: an accepted connection inherits the listener's decoder. We prove the
    // decoder is wired into the connection pipeline without breaking it. Snapshots
    // exist for data channels, but per-connection snapshots are deferred (the
    // supervisor keeps no per-connection handle), so we assert the Message flows.
    let port = free_tcp_port();
    let mut config = templates::tcp_listener_template();
    if let InterfaceConfig::TcpListener(tcp) = &mut config.interface {
        tcp.bind_address = "127.0.0.1".to_string();
        tcp.port = port;
    }
    config.extraction = ExtractionConfig::Delimiter {
        delimiter: vec![b'\r', b'\n'],
        include_delimiter: false,
    };
    config.decoder = DecoderConfig::Nmea0183 {
        validation_mode: NmeaValidationMode::Standard,
    };

    let mut listener = Listener::with_default_capacities();
    let mut events = listener.take_events().unwrap();
    let id = listener.add_channel(config);
    listener.start(id).await.unwrap();

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let conn_id = next_connection(&mut events).await;

    // A CRLF-terminated NMEA sentence is extracted and decoded; the Message is
    // delivered regardless of the checksum outcome (§37 never hides data).
    client.write_all(b"$GPGLL,4916.45,N*00\r\n").await.unwrap();
    assert_eq!(next_message(&mut events).await, (conn_id, 1));

    stop(&mut listener, id).await;
}
