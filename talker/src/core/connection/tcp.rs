use std::io::Write;
use std::net::TcpStream;

use anyhow::Context;

use super::config::TcpClientConfig;
use super::Connection;

pub(super) struct TcpClientConnection {
    stream: TcpStream,
}

impl TcpClientConnection {
    pub(super) fn open(config: &TcpClientConfig) -> anyhow::Result<Self> {
        let stream = TcpStream::connect(config.address)
            .with_context(|| format!("connecting to {}", config.address))?;
        Ok(Self { stream })
    }
}

impl Connection for TcpClientConnection {
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
        let result = TcpClientConnection::open(&config);
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
        let mut conn = TcpClientConnection::open(&config).unwrap();

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
    fn send_large_payload() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let config = TcpClientConfig::new(addr);
        let mut conn = TcpClientConnection::open(&config).unwrap();

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
