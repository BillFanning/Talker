use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use anyhow::Context;

use super::config::TcpClientConfig;
use super::Interface;

/// Cap on connect. Without it the OS default applies (~20s on Windows),
/// which is far too long for an interactive tool to sit unresponsive.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on each blocking write. Without it, a peer that stops reading
/// (dead device, full window) wedges the owning talker thread inside
/// `write_all` forever — it can then never see its Stop command.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct TcpClientInterface {
    stream: TcpStream,
}

impl TcpClientInterface {
    pub(super) fn open(config: &TcpClientConfig) -> anyhow::Result<Self> {
        let stream = TcpStream::connect_timeout(&config.address, CONNECT_TIMEOUT)
            .with_context(|| format!("connecting to {}", config.address))?;
        stream
            .set_write_timeout(Some(WRITE_TIMEOUT))
            .context("setting TCP write timeout")?;
        // Talker is a timing-oriented test tool: each scheduled message
        // should hit the wire when it fires, not when Nagle decides to
        // coalesce it with the next one.
        stream.set_nodelay(true).context("setting TCP_NODELAY")?;
        Ok(Self { stream })
    }
}

impl Interface for TcpClientInterface {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.stream.write_all(data).context("writing to TCP stream")
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn connect_to_nonexistent_port_returns_error() {
        let config = TcpClientConfig::new("127.0.0.1:1".parse().unwrap());
        let result = TcpClientInterface::open(&config);
        let msg = result
            .err()
            .expect("expected error connecting to port 1")
            .to_string();
        assert!(msg.contains("127.0.0.1:1"));
    }

    #[test]
    fn send_to_local_listener() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = TcpClientConfig::new(addr);
        let mut conn = TcpClientInterface::open(&config).unwrap();

        let (mut server_stream, _) = listener.accept().unwrap();

        conn.send(b"ping").unwrap();

        let mut buf = [0u8; 16];
        use std::io::Read;
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let n = server_stream.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"ping");
    }

    #[test]
    fn open_sets_nodelay_and_write_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let conn = TcpClientInterface::open(&TcpClientConfig::new(addr)).unwrap();
        assert!(conn.stream.nodelay().unwrap());
        assert_eq!(conn.stream.write_timeout().unwrap(), Some(WRITE_TIMEOUT));
    }

    #[test]
    fn send_large_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = TcpClientConfig::new(addr);
        let mut conn = TcpClientInterface::open(&config).unwrap();

        let (mut server_stream, _) = listener.accept().unwrap();
        let payload = vec![0xABu8; 4096];

        conn.send(&payload).unwrap();

        let mut received = Vec::new();
        server_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut buf = [0u8; 4096];
        loop {
            match std::io::Read::read(&mut server_stream, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
            if received.len() >= payload.len() {
                break;
            }
        }
        assert_eq!(received, payload);
    }
}
