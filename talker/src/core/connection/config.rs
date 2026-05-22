use std::net::{Ipv4Addr, SocketAddr};

use serde::{Deserialize, Serialize};

/// Top-level discriminator stored in a profile's connection list.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionConfig {
    Serial(SerialConfig),
    Udp(UdpConfig),
    TcpClient(TcpClientConfig),
}

// ── Serial ────────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerialConfig {
    pub port: String,
    #[serde(default = "default_baud")]
    pub baud_rate: u32,
    #[serde(default)]
    pub data_bits: DataBits,
    #[serde(default)]
    pub parity: Parity,
    #[serde(default)]
    pub stop_bits: StopBits,
    #[serde(default)]
    pub flow_control: FlowControl,
}

fn default_baud() -> u32 {
    9600
}

impl SerialConfig {
    pub fn new(port: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            baud_rate: default_baud(),
            data_bits: DataBits::default(),
            parity: Parity::default(),
            stop_bits: StopBits::default(),
            flow_control: FlowControl::default(),
        }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    #[default]
    None,
    Odd,
    Even,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopBits {
    #[default]
    One,
    Two,
}

#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    #[default]
    None,
    Software,
    Hardware,
}

// ── UDP ───────────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpConfig {
    pub mode: UdpMode,
    /// Local port to bind; `None` lets the OS choose an ephemeral port.
    #[serde(default)]
    pub local_port: Option<u16>,
}

impl UdpConfig {
    pub fn unicast(destination: SocketAddr) -> Self {
        Self { mode: UdpMode::Unicast { destination }, local_port: None }
    }

    pub fn broadcast(destination: SocketAddr) -> Self {
        Self { mode: UdpMode::Broadcast { destination }, local_port: None }
    }

    pub fn multicast(group: Ipv4Addr, port: u16) -> Self {
        Self { mode: UdpMode::Multicast { group, port, interface: None }, local_port: None }
    }
}

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UdpMode {
    Unicast { destination: SocketAddr },
    Broadcast { destination: SocketAddr },
    Multicast {
        group: Ipv4Addr,
        port: u16,
        /// Outgoing interface; `None` means OS default.
        #[serde(default)]
        interface: Option<Ipv4Addr>,
    },
}

// ── TCP client ────────────────────────────────────────────────────────────────

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TcpClientConfig {
    pub address: SocketAddr,
}

impl TcpClientConfig {
    pub fn new(address: SocketAddr) -> Self {
        Self { address }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(
        value: &T,
    ) {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back);
    }

    #[test]
    fn serial_config_defaults() {
        let c = SerialConfig::new("COM1");
        assert_eq!(c.baud_rate, 9600);
        assert_eq!(c.data_bits, DataBits::Eight);
        assert_eq!(c.parity, Parity::None);
        assert_eq!(c.stop_bits, StopBits::One);
        assert_eq!(c.flow_control, FlowControl::None);
    }

    #[test]
    fn serial_config_round_trip() {
        let c = SerialConfig::new("/dev/ttyUSB0");
        round_trip(&c);
    }

    #[test]
    fn connection_config_serial_tag() {
        let c = ConnectionConfig::Serial(SerialConfig::new("COM3"));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"serial\""));
        round_trip(&c);
    }

    #[test]
    fn udp_unicast_round_trip() {
        let c = ConnectionConfig::Udp(UdpConfig::unicast("127.0.0.1:5000".parse().unwrap()));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"udp\""));
        assert!(json.contains("\"type\":\"unicast\""));
        round_trip(&c);
    }

    #[test]
    fn udp_broadcast_round_trip() {
        let c = ConnectionConfig::Udp(UdpConfig::broadcast(
            "255.255.255.255:9999".parse().unwrap(),
        ));
        round_trip(&c);
    }

    #[test]
    fn udp_multicast_round_trip() {
        let c = ConnectionConfig::Udp(UdpConfig::multicast("239.1.2.3".parse().unwrap(), 5000));
        round_trip(&c);
    }

    #[test]
    fn tcp_client_round_trip() {
        let c = ConnectionConfig::TcpClient(TcpClientConfig::new("10.0.0.1:4001".parse().unwrap()));
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"type\":\"tcp_client\""));
        round_trip(&c);
    }

    #[test]
    fn data_bits_default_is_eight() {
        assert_eq!(DataBits::default(), DataBits::Eight);
    }

    #[test]
    fn parity_default_is_none() {
        assert_eq!(Parity::default(), Parity::None);
    }

    #[test]
    fn stop_bits_default_is_one() {
        assert_eq!(StopBits::default(), StopBits::One);
    }

    #[test]
    fn flow_control_default_is_none() {
        assert_eq!(FlowControl::default(), FlowControl::None);
    }
}
