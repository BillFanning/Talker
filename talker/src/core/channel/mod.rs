mod config;
mod serial;
mod tcp;
mod udp;

pub use config::{
    ChannelConfig, DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits,
    TcpClientConfig, UdpConfig, UdpMode,
};

/// A live interface that can send raw bytes.
///
/// An interface is owned by a single talker thread; `Send` is required so it
/// can be moved into that thread.
pub trait Interface: Send {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()>;
}

impl InterfaceConfig {
    /// Open the live interface described by this config.
    pub fn open(&self) -> anyhow::Result<Box<dyn Interface>> {
        match self {
            Self::Serial(c) => Ok(Box::new(serial::SerialInterface::open(c)?)),
            Self::Udp(c) => Ok(Box::new(udp::UdpInterface::open(c)?)),
            Self::TcpClient(c) => Ok(Box::new(tcp::TcpClientInterface::open(c)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;

    use super::*;

    #[test]
    fn open_udp_unicast_succeeds() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = InterfaceConfig::Udp(UdpConfig::unicast(receiver.local_addr().unwrap()));
        assert!(cfg.open().is_ok());
    }

    #[test]
    fn open_serial_bad_port_returns_error() {
        let cfg = InterfaceConfig::Serial(SerialConfig::new("/dev/does_not_exist_xyz"));
        assert!(cfg.open().is_err());
    }
}
