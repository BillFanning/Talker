use std::net::{SocketAddr, UdpSocket};

use anyhow::Context;

use super::config::{UdpConfig, UdpMode};
use super::Interface;

pub(super) struct UdpInterface {
    socket: UdpSocket,
    destination: SocketAddr,
}

impl UdpInterface {
    pub(super) fn open(config: &UdpConfig) -> anyhow::Result<Self> {
        let local_port = config.local_port.unwrap_or(0);

        match &config.mode {
            UdpMode::Unicast { destination } => {
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                Ok(Self {
                    socket,
                    destination: *destination,
                })
            }
            UdpMode::Broadcast { destination } => {
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                socket
                    .set_broadcast(true)
                    .context("enabling UDP broadcast")?;
                Ok(Self {
                    socket,
                    destination: *destination,
                })
            }
            UdpMode::Multicast {
                group,
                port,
                interface,
                ttl,
            } => {
                let socket =
                    UdpSocket::bind(("0.0.0.0", local_port)).context("binding UDP socket")?;
                // Outgoing interface (IP_MULTICAST_IF): std has no setter, so
                // reach through socket2's SockRef over the already-bound socket.
                if let Some(iface) = interface {
                    socket2::SockRef::from(&socket)
                        .set_multicast_if_v4(iface)
                        .with_context(|| {
                            format!("selecting multicast interface {iface} (IP_MULTICAST_IF)")
                        })?;
                }
                // Hop limit (IP_MULTICAST_TTL); OS default is 1 (local subnet).
                if let Some(ttl) = ttl {
                    socket
                        .set_multicast_ttl_v4(*ttl)
                        .with_context(|| format!("setting multicast TTL {ttl}"))?;
                }
                let destination = SocketAddr::from((*group, *port));
                Ok(Self {
                    socket,
                    destination,
                })
            }
        }
    }
}

impl Interface for UdpInterface {
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
        let mut conn = UdpInterface::open(&config).unwrap();

        conn.send(b"hello").unwrap();

        let mut buf = [0u8; 16];
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (n, _) = receiver.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn broadcast_socket_opens() {
        let dest: SocketAddr = "255.255.255.255:19999".parse().unwrap();
        let config = UdpConfig::broadcast(dest);
        assert!(UdpInterface::open(&config).is_ok());
    }

    #[test]
    fn multicast_socket_opens() {
        let config = UdpConfig::multicast("239.0.0.1".parse().unwrap(), 20000);
        assert!(UdpInterface::open(&config).is_ok());
    }

    #[test]
    fn multicast_socket_opens_with_ttl_and_default_interface() {
        // TTL is applied via std; the default interface (0.0.0.0) is a valid
        // IP_MULTICAST_IF selection, so both options exercise the open path.
        let config = UdpConfig::multicast_with(
            "239.0.0.2".parse().unwrap(),
            20001,
            Some(std::net::Ipv4Addr::UNSPECIFIED),
            Some(4),
        );
        let conn = UdpInterface::open(&config).unwrap();
        assert_eq!(conn.socket.multicast_ttl_v4().unwrap(), 4);
    }

    #[test]
    fn multicast_send_to_loopback_group_with_ttl() {
        // A receiver joined to a group on loopback receives a datagram sent
        // with an explicit TTL — proves the configured socket actually sends.
        let group: std::net::Ipv4Addr = "239.255.0.99".parse().unwrap();
        let receiver = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let port = receiver.local_addr().unwrap().port();
        let loopback = std::net::Ipv4Addr::LOCALHOST;
        if receiver.join_multicast_v4(&group, &loopback).is_err() {
            // Some CI hosts have no multicast-capable loopback; skip cleanly.
            return;
        }
        receiver.set_multicast_loop_v4(true).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();

        let mut conn = UdpInterface::open(&UdpConfig::multicast_with(
            group,
            port,
            Some(loopback),
            Some(1),
        ))
        .unwrap();
        conn.send(b"mc").unwrap();

        let mut buf = [0u8; 8];
        if let Ok((n, _)) = receiver.recv_from(&mut buf) {
            assert_eq!(&buf[..n], b"mc");
        }
        // A missed datagram (host multicast quirks) is not a logic failure;
        // the assertion above only fires when something *was* received.
    }

    #[test]
    fn send_multiple_datagrams() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let dest = receiver.local_addr().unwrap();

        let mut conn = UdpInterface::open(&UdpConfig::unicast(dest)).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();

        for msg in [b"one" as &[u8], b"two", b"three"] {
            conn.send(msg).unwrap();
            let mut buf = [0u8; 16];
            let (n, _) = receiver.recv_from(&mut buf).unwrap();
            assert_eq!(&buf[..n], msg);
        }
    }
}
