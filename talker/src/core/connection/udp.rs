use std::net::{SocketAddr, UdpSocket};

use anyhow::Context;

use super::Connection;
use super::config::{UdpConfig, UdpMode};

pub(super) struct UdpConnection {
    socket: UdpSocket,
    destination: SocketAddr,
}

impl UdpConnection {
    pub(super) fn open(config: &UdpConfig) -> anyhow::Result<Self> {
        let local_port = config.local_port.unwrap_or(0);

        match &config.mode {
            UdpMode::Unicast { destination } => {
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                Ok(Self { socket, destination: *destination })
            }
            UdpMode::Broadcast { destination } => {
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                socket.set_broadcast(true).context("enabling UDP broadcast")?;
                Ok(Self { socket, destination: *destination })
            }
            UdpMode::Multicast { group, port, interface: _ } => {
                // Outgoing interface selection requires socket2; for now the OS
                // default interface is used.  The config field is preserved for
                // future wiring.
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                let destination = SocketAddr::from((*group, *port));
                Ok(Self { socket, destination })
            }
        }
    }
}

impl Connection for UdpConnection {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.socket
            .send_to(data, self.destination)
            .context("sending UDP datagram")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicast_send_loopback() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest: SocketAddr = receiver.local_addr().unwrap();

        let config = UdpConfig::unicast(dest);
        let mut conn = UdpConnection::open(&config).unwrap();

        conn.send(b"hello").unwrap();

        let mut buf = [0u8; 16];
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn broadcast_socket_opens() {
        let dest: SocketAddr = "255.255.255.255:19999".parse().unwrap();
        let config = UdpConfig::broadcast(dest);
        assert!(UdpConnection::open(&config).is_ok());
    }

    #[test]
    fn multicast_socket_opens() {
        let config = UdpConfig::multicast("239.0.0.1".parse().unwrap(), 20000);
        assert!(UdpConnection::open(&config).is_ok());
    }

    #[test]
    fn send_multiple_datagrams() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = receiver.local_addr().unwrap();

        let mut conn = UdpConnection::open(&UdpConfig::unicast(dest)).unwrap();
        receiver.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();

        for msg in [b"one" as &[u8], b"two", b"three"] {
            conn.send(msg).unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = receiver.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], msg);
        }
    }
}
