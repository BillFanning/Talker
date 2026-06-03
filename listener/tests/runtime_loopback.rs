//! Integration tests: loopback UDP/TCP channels driven through the runtime
//! orchestrator (spec §153–§155). Black-box — public API only.

use std::time::Duration;

use listener::config::{templates, DecoderConfig, ExtractionConfig, InterfaceConfig};
use listener::core::{ChannelId, ChannelState, RuntimeEvent};
use listener::decode::NmeaValidationMode;
use listener::runtime::Listener;
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
async fn tcp_connection_inherits_the_listener_decoder() {
    // §16.2: an accepted connection inherits the listener's decoder. We prove the
    // decoder is wired into the connection pipeline without breaking it — the
    // decoded metadata isn't observable via the public API yet (snapshot API is
    // future work), so we assert the Message still flows end-to-end.
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
