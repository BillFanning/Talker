//! UDP transport: unicast, broadcast, and multicast (spec §15).
//!
//! A UDP transport runs as a Tokio task (ADR-001 — async-native socket I/O, no
//! dedicated thread). Each received datagram is delivered whole by the OS (§15):
//! the transport emits `ReceivedPayload::Datagram`, which the pipeline appends to
//! the verbatim stream like any other bytes (ADR-010 — no extraction, the datagram
//! boundary is a reception detail, not stream structure).
//!
//! Binding is separated from the receive loop so resource errors surface at
//! Channel Start (§71, §8.2): [`UdpTransport::bind`] performs the fallible
//! socket setup and returns a [`BoundUdpTransport`] whose infallible
//! [`DataTransportRunner::run`] spawns the loop (§138).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, ChunkTime};

use super::{
    DataTransportRunner, ReceivedData, ReceivedPayload, TransportJoinHandle, TransportOutcome,
};

/// Maximum size of a single UDP datagram payload (IPv4 theoretical max). The
/// receive buffer is sized to this so no datagram is ever truncated.
const MAX_DATAGRAM: usize = 65_535;

/// How a UDP Channel receives (§15, mirrors `UdpMode` in the profile schema §75).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UdpMode {
    Unicast,
    Broadcast,
    Multicast,
}

/// An unbound UDP transport description (§15, §75). Call [`bind`](Self::bind) at
/// Channel Start to acquire the socket.
#[derive(Clone, Debug)]
pub struct UdpTransport {
    channel_id: ChannelId,
    bind_addr: SocketAddr,
    mode: UdpMode,
    multicast_group: Option<IpAddr>,
    multicast_interface: Option<Ipv4Addr>,
    recv_buffer: Option<usize>,
}

impl UdpTransport {
    pub fn new(channel_id: ChannelId, bind_addr: SocketAddr, mode: UdpMode) -> Self {
        Self {
            channel_id,
            bind_addr,
            mode,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer: None,
        }
    }

    /// Set the multicast group to join (required for [`UdpMode::Multicast`]).
    pub fn with_multicast_group(mut self, group: IpAddr) -> Self {
        self.multicast_group = Some(group);
        self
    }

    /// Select the local IPv4 interface to join the multicast group on (§167), for
    /// multi-homed hosts. Without it, the OS default interface is used.
    pub fn with_multicast_interface(mut self, interface: Ipv4Addr) -> Self {
        self.multicast_interface = Some(interface);
        self
    }

    /// Set SO_RCVBUF in bytes (§167) — the lever against kernel-dropped UDP (§101).
    pub fn with_recv_buffer(mut self, bytes: usize) -> Self {
        self.recv_buffer = Some(bytes);
        self
    }

    /// Bind the socket and apply mode-specific setup (§8.2). Fallible so the
    /// runtime can take Starting → Faulted on failure (§9, §71).
    pub async fn bind(self) -> io::Result<BoundUdpTransport> {
        let std_socket = build_udp_socket(self.bind_addr, self.recv_buffer)?;
        std_socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_socket)?;
        match self.mode {
            UdpMode::Unicast => {}
            UdpMode::Broadcast => socket.set_broadcast(true)?,
            UdpMode::Multicast => {
                let group = self.multicast_group.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "multicast mode requires a multicast group address",
                    )
                })?;
                match group {
                    IpAddr::V4(group) => {
                        let iface = self.multicast_interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
                        socket.join_multicast_v4(group, iface)?;
                    }
                    IpAddr::V6(group) => socket.join_multicast_v6(&group, 0)?,
                }
            }
        }
        Ok(BoundUdpTransport {
            channel_id: self.channel_id,
            socket,
        })
    }
}

/// Build and bind a std UDP socket, optionally setting SO_RCVBUF *before* bind
/// (§167) — the point most platforms honor. Built via `socket2` so the option can
/// be set on the raw socket before it is handed to Tokio.
fn build_udp_socket(
    addr: SocketAddr,
    recv_buffer: Option<usize>,
) -> io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if let Some(bytes) = recv_buffer {
        socket.set_recv_buffer_size(bytes)?;
    }
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// A bound UDP socket ready to receive. Implements [`DataTransportRunner`].
#[derive(Debug)]
pub struct BoundUdpTransport {
    channel_id: ChannelId,
    socket: UdpSocket,
}

impl BoundUdpTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// The actual bound address (useful when binding to an ephemeral port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl DataTransportRunner for BoundUdpTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let BoundUdpTransport { channel_id, socket } = self;
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            loop {
                tokio::select! {
                    biased;
                    // Cooperative cancellation, checked first (§111).
                    _ = cancel.cancelled() => return TransportOutcome::Cancelled,
                    res = socket.recv_from(&mut buf) => match res {
                        Ok((n, _from)) => {
                            let data = ReceivedData {
                                channel_id,
                                payload: ReceivedPayload::Datagram(buf[..n].to_vec()),
                                received_at: ChunkTime::now(),
                            };
                            // Awaiting `send` is the one place this transport may
                            // stall: a full Transport→Pipeline queue backpressures the
                            // recv loop, which can drop datagrams at the kernel (§97.1,
                            // §101). An `Err` means the pipeline is gone.
                            if out.send(data).await.is_err() {
                                return TransportOutcome::Completed;
                            }
                        }
                        // A receive error ends the transport as a fault (§94). Some
                        // loss (e.g. kernel-dropped datagrams) is not observable
                        // from userland; we report the failure we can see (§101).
                        Err(e) => return TransportOutcome::Faulted(format!("UDP receive failed: {e}")),
                    },
                }
            }
        });
        TransportJoinHandle::Task(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recv_buffer_size_is_applied_at_bind() {
        // §167: SO_RCVBUF is set before bind. The OS may round up (Linux commonly
        // doubles), so the effective size is at least what we requested.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let requested = 512 * 1024;
        let socket = build_udp_socket(addr, Some(requested)).unwrap();
        let actual = socket2::SockRef::from(&socket).recv_buffer_size().unwrap();
        assert!(
            actual >= requested,
            "recv buffer {actual} should be >= requested {requested}"
        );
    }

    #[tokio::test]
    async fn unicast_receives_each_datagram_as_a_chunk() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "127.0.0.1:0".parse().unwrap(),
            UdpMode::Unicast,
        );
        let bound = transport.bind().await.unwrap();
        let server_addr = bound.local_addr().unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let cancel = CancellationToken::new();
        let handle = bound.run(tx, cancel.clone());

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"$GPGGA,first", server_addr).await.unwrap();
        client.send_to(b"second", server_addr).await.unwrap();

        // `recv().await` blocks until the transport emits — deterministic, no sleep.
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.payload.bytes(), b"$GPGGA,first");
        assert_eq!(second.payload.bytes(), b"second");
        assert!(matches!(first.payload, ReceivedPayload::Datagram(_)));

        cancel.cancel();
        assert!(matches!(handle.join().await, TransportOutcome::Cancelled));
    }

    #[tokio::test]
    async fn broadcast_mode_binds() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Broadcast,
        );
        assert!(transport.bind().await.is_ok());
    }

    #[tokio::test]
    async fn multicast_mode_joins_the_group() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Multicast,
        )
        .with_multicast_group("239.0.0.1".parse().unwrap());
        assert!(transport.bind().await.is_ok());
    }

    #[tokio::test]
    async fn multicast_without_group_is_rejected() {
        let transport = UdpTransport::new(
            ChannelId::new(),
            "0.0.0.0:0".parse().unwrap(),
            UdpMode::Multicast,
        );
        let err = transport.bind().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
