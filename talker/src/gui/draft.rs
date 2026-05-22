use std::net::{Ipv4Addr, SocketAddr};

use crate::core::{
    connection::{
        ConnectionConfig, DataBits, FlowControl, Parity, SerialConfig, StopBits, TcpClientConfig,
        UdpConfig, UdpMode,
    },
    scheduler::{PayloadConfig, ScheduleEntryConfig},
};

// ── Connection ────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum ConnKind {
    Serial,
    Udp,
    Tcp,
}

#[derive(PartialEq, Clone, Copy)]
pub enum UdpModeDraft {
    Unicast,
    Broadcast,
    Multicast,
}

pub struct ConnDraft {
    pub kind: ConnKind,
    // serial
    pub serial_port: String,
    pub baud_rate: u32,
    pub baud_custom: String, // text buffer for non-preset baud rates
    pub data_bits: u8,
    pub parity: u8,       // 0=None 1=Odd 2=Even
    pub stop_bits: u8,    // 1 or 2
    pub flow_control: u8, // 0=None 1=Software 2=Hardware
    // udp
    pub udp_mode: UdpModeDraft,
    pub udp_dest: String,    // unicast / broadcast destination (host:port)
    pub udp_group: String,   // multicast group address
    pub udp_mc_port: String, // multicast port
    pub local_port: String,  // optional local bind port
    // tcp
    pub tcp_addr: String,
}

impl Default for ConnDraft {
    fn default() -> Self {
        Self {
            kind: ConnKind::Serial,
            serial_port: String::new(),
            baud_rate: 9600,
            baud_custom: String::new(),
            data_bits: 8,
            parity: 0,
            stop_bits: 1,
            flow_control: 0,
            udp_mode: UdpModeDraft::Unicast,
            udp_dest: String::new(),
            udp_group: String::new(),
            udp_mc_port: String::new(),
            local_port: String::new(),
            tcp_addr: String::new(),
        }
    }
}

impl From<&ConnectionConfig> for ConnDraft {
    fn from(cfg: &ConnectionConfig) -> Self {
        match cfg {
            ConnectionConfig::Serial(s) => {
                const PRESETS: &[u32] = &[4800, 9600, 19200, 38400, 57600, 115200];
                Self {
                    kind: ConnKind::Serial,
                    serial_port: s.port.clone(),
                    baud_rate: s.baud_rate,
                    baud_custom: if PRESETS.contains(&s.baud_rate) {
                        String::new()
                    } else {
                        s.baud_rate.to_string()
                    },
                    data_bits: match s.data_bits {
                        DataBits::Five => 5,
                        DataBits::Six => 6,
                        DataBits::Seven => 7,
                        _ => 8,
                    },
                    parity: match s.parity {
                        Parity::Odd => 1,
                        Parity::Even => 2,
                        _ => 0,
                    },
                    stop_bits: match s.stop_bits {
                        StopBits::Two => 2,
                        _ => 1,
                    },
                    flow_control: match s.flow_control {
                        FlowControl::Software => 1,
                        FlowControl::Hardware => 2,
                        _ => 0,
                    },
                    ..Default::default()
                }
            }
            ConnectionConfig::Udp(u) => {
                let (udp_mode, udp_dest, udp_group, udp_mc_port) = match &u.mode {
                    UdpMode::Unicast { destination } => (
                        UdpModeDraft::Unicast,
                        destination.to_string(),
                        String::new(),
                        String::new(),
                    ),
                    UdpMode::Broadcast { destination } => (
                        UdpModeDraft::Broadcast,
                        destination.to_string(),
                        String::new(),
                        String::new(),
                    ),
                    UdpMode::Multicast { group, port, .. } => (
                        UdpModeDraft::Multicast,
                        String::new(),
                        group.to_string(),
                        port.to_string(),
                    ),
                };
                Self {
                    kind: ConnKind::Udp,
                    udp_mode,
                    udp_dest,
                    udp_group,
                    udp_mc_port,
                    local_port: u.local_port.map(|p| p.to_string()).unwrap_or_default(),
                    ..Default::default()
                }
            }
            ConnectionConfig::TcpClient(t) => Self {
                kind: ConnKind::Tcp,
                tcp_addr: t.address.to_string(),
                ..Default::default()
            },
        }
    }
}

impl ConnDraft {
    pub fn to_config(&self) -> Option<ConnectionConfig> {
        let local_port = self.local_port.parse::<u16>().ok();
        match self.kind {
            ConnKind::Serial => {
                if self.serial_port.is_empty() {
                    return None;
                }
                Some(ConnectionConfig::Serial(SerialConfig {
                    port: self.serial_port.clone(),
                    baud_rate: self.baud_rate,
                    data_bits: match self.data_bits {
                        5 => DataBits::Five,
                        6 => DataBits::Six,
                        7 => DataBits::Seven,
                        _ => DataBits::Eight,
                    },
                    parity: match self.parity {
                        1 => Parity::Odd,
                        2 => Parity::Even,
                        _ => Parity::None,
                    },
                    stop_bits: if self.stop_bits == 2 {
                        StopBits::Two
                    } else {
                        StopBits::One
                    },
                    flow_control: match self.flow_control {
                        1 => FlowControl::Software,
                        2 => FlowControl::Hardware,
                        _ => FlowControl::None,
                    },
                }))
            }
            ConnKind::Udp => {
                let mode = match self.udp_mode {
                    UdpModeDraft::Unicast => {
                        let dest: SocketAddr = self.udp_dest.parse().ok()?;
                        UdpMode::Unicast { destination: dest }
                    }
                    UdpModeDraft::Broadcast => {
                        let dest: SocketAddr = self.udp_dest.parse().ok()?;
                        UdpMode::Broadcast { destination: dest }
                    }
                    UdpModeDraft::Multicast => {
                        let group: Ipv4Addr = self.udp_group.parse().ok()?;
                        let port: u16 = self.udp_mc_port.parse().ok()?;
                        UdpMode::Multicast {
                            group,
                            port,
                            interface: None,
                        }
                    }
                };
                Some(ConnectionConfig::Udp(UdpConfig { mode, local_port }))
            }
            ConnKind::Tcp => {
                let addr: SocketAddr = self.tcp_addr.parse().ok()?;
                Some(ConnectionConfig::TcpClient(TcpClientConfig {
                    address: addr,
                }))
            }
        }
    }
}

// ── Schedule ──────────────────────────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy)]
pub enum PayloadKind {
    RawHex,
    Nmea,
}

pub struct ScheduleDraft {
    pub payload_kind: PayloadKind,
    // raw hex
    pub hex_data: String,
    // nmea
    pub nmea_talker: String,
    pub nmea_sentence_type: String,
    pub nmea_fields: String, // comma-separated field values
    // common
    pub interval_ms: String,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        Self {
            payload_kind: PayloadKind::RawHex,
            hex_data: String::new(),
            nmea_talker: "GP".to_string(),
            nmea_sentence_type: String::new(),
            nmea_fields: String::new(),
            interval_ms: "1000".to_string(),
        }
    }
}

impl From<&ScheduleEntryConfig> for ScheduleDraft {
    fn from(e: &ScheduleEntryConfig) -> Self {
        match &e.payload {
            PayloadConfig::RawHex { data } => Self {
                payload_kind: PayloadKind::RawHex,
                hex_data: data.clone(),
                interval_ms: e.interval_ms.to_string(),
                ..Default::default()
            },
            PayloadConfig::Nmea {
                talker,
                sentence_type,
                fields,
            } => Self {
                payload_kind: PayloadKind::Nmea,
                nmea_talker: talker.clone(),
                nmea_sentence_type: sentence_type.clone(),
                nmea_fields: fields.join(","),
                interval_ms: e.interval_ms.to_string(),
                ..Default::default()
            },
        }
    }
}

impl ScheduleDraft {
    pub fn to_entry_config(&self) -> Option<ScheduleEntryConfig> {
        let interval_ms: u64 = self.interval_ms.parse().ok()?;
        let payload = match self.payload_kind {
            PayloadKind::RawHex => PayloadConfig::raw_hex(&self.hex_data),
            PayloadKind::Nmea => {
                if self.nmea_talker.is_empty() || self.nmea_sentence_type.is_empty() {
                    return None;
                }
                let fields: Vec<String> = if self.nmea_fields.is_empty() {
                    vec![]
                } else {
                    self.nmea_fields.split(',').map(str::to_string).collect()
                };
                PayloadConfig::nmea(&self.nmea_talker, &self.nmea_sentence_type, fields)
            }
        };
        Some(ScheduleEntryConfig::new(payload, interval_ms))
    }
}
