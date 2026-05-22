use std::net::{Ipv4Addr, SocketAddr};

use crate::core::{
    channel::{
        DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits, TcpClientConfig,
        UdpConfig, UdpMode,
    },
    message::{
        ByteOrder, ChecksumAlgorithm, ChecksumConfig, CodePage, MessageConfig, PayloadConfig,
        TimestampConfig,
    },
};

// ── Channel interface ─────────────────────────────────────────────────────────

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

impl From<&InterfaceConfig> for ConnDraft {
    fn from(cfg: &InterfaceConfig) -> Self {
        match cfg {
            InterfaceConfig::Serial(s) => {
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
            InterfaceConfig::Udp(u) => {
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
            InterfaceConfig::TcpClient(t) => Self {
                kind: ConnKind::Tcp,
                tcp_addr: t.address.to_string(),
                ..Default::default()
            },
        }
    }
}

impl ConnDraft {
    pub fn to_config(&self) -> Option<InterfaceConfig> {
        let local_port = self.local_port.parse::<u16>().ok();
        match self.kind {
            ConnKind::Serial => {
                if self.serial_port.is_empty() {
                    return None;
                }
                Some(InterfaceConfig::Serial(SerialConfig {
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
                let mut udp = match self.udp_mode {
                    UdpModeDraft::Unicast => UdpConfig::unicast(self.udp_dest.parse().ok()?),
                    UdpModeDraft::Broadcast => UdpConfig::broadcast(self.udp_dest.parse().ok()?),
                    UdpModeDraft::Multicast => {
                        let group: Ipv4Addr = self.udp_group.parse().ok()?;
                        let port: u16 = self.udp_mc_port.parse().ok()?;
                        UdpConfig::multicast(group, port)
                    }
                };
                udp.local_port = local_port;
                Some(InterfaceConfig::Udp(udp))
            }
            ConnKind::Tcp => {
                let addr: SocketAddr = self.tcp_addr.parse().ok()?;
                Some(InterfaceConfig::TcpClient(TcpClientConfig::new(addr)))
            }
        }
    }
}

// ── Message ───────────────────────────────────────────────────────────────────

/// Which payload format a message draft is editing.
#[derive(PartialEq, Clone, Copy)]
pub enum PayloadKind {
    Hex,
    Utf8,
    Utf16,
    Ascii,
    Nmea,
}

/// Editable state for one message. A text buffer is kept per format so
/// switching the format selector does not discard what was typed.
pub struct ScheduleDraft {
    pub payload_kind: PayloadKind,
    // hex
    pub hex_data: String,
    // utf-8
    pub utf8_text: String,
    // utf-16
    pub utf16_text: String,
    pub utf16_big_endian: bool,
    pub utf16_bom: bool,
    // ascii
    pub ascii_text: String,
    pub ascii_code_page: CodePage,
    // nmea
    pub nmea_talker: String,
    pub nmea_sentence_type: String,
    pub nmea_fields: String, // comma-separated field values
    // common
    pub interval_ms: String,
    // timestamp
    pub timestamp_enabled: bool,
    pub ts_date: bool,
    pub ts_millis: bool,
    pub ts_timezone: bool,
    // checksum
    pub checksum_enabled: bool,
    pub checksum_algorithm: ChecksumAlgorithm,
    pub checksum_wrong: bool,
}

impl Default for ScheduleDraft {
    fn default() -> Self {
        Self {
            payload_kind: PayloadKind::Hex,
            hex_data: String::new(),
            utf8_text: String::new(),
            utf16_text: String::new(),
            utf16_big_endian: true,
            utf16_bom: false,
            ascii_text: String::new(),
            ascii_code_page: CodePage::default(),
            nmea_talker: "GP".to_string(),
            nmea_sentence_type: String::new(),
            nmea_fields: String::new(),
            interval_ms: "1000".to_string(),
            timestamp_enabled: false,
            ts_date: true,
            ts_millis: false,
            ts_timezone: true,
            checksum_enabled: false,
            checksum_algorithm: ChecksumAlgorithm::default(),
            checksum_wrong: false,
        }
    }
}

impl From<&MessageConfig> for ScheduleDraft {
    fn from(m: &MessageConfig) -> Self {
        let mut d = ScheduleDraft::default();
        match &m.payload {
            PayloadConfig::RawHex { data } => {
                d.payload_kind = PayloadKind::Hex;
                d.hex_data = data.clone();
            }
            PayloadConfig::Utf8 { text } => {
                d.payload_kind = PayloadKind::Utf8;
                d.utf8_text = text.clone();
            }
            PayloadConfig::Utf16 {
                text,
                byte_order,
                bom,
            } => {
                d.payload_kind = PayloadKind::Utf16;
                d.utf16_text = text.clone();
                d.utf16_big_endian = matches!(byte_order, ByteOrder::BigEndian);
                d.utf16_bom = *bom;
            }
            PayloadConfig::Ascii { text, code_page } => {
                d.payload_kind = PayloadKind::Ascii;
                d.ascii_text = text.clone();
                d.ascii_code_page = *code_page;
            }
            PayloadConfig::Nmea {
                talker,
                sentence_type,
                fields,
            } => {
                d.payload_kind = PayloadKind::Nmea;
                d.nmea_talker = talker.clone();
                d.nmea_sentence_type = sentence_type.clone();
                d.nmea_fields = fields.join(",");
            }
        }
        d.interval_ms = m.interval_ms.to_string();
        if let Some(ts) = &m.timestamp {
            d.timestamp_enabled = true;
            d.ts_date = ts.include_date;
            d.ts_millis = ts.include_millis;
            d.ts_timezone = ts.include_timezone;
        }
        if let Some(cs) = &m.checksum {
            d.checksum_enabled = true;
            d.checksum_algorithm = cs.algorithm;
            d.checksum_wrong = cs.intentionally_wrong;
        }
        d
    }
}

impl ScheduleDraft {
    pub fn to_message_config(&self) -> Option<MessageConfig> {
        let interval_ms: u64 = self.interval_ms.parse().ok()?;
        let payload = match self.payload_kind {
            PayloadKind::Hex => PayloadConfig::raw_hex(&self.hex_data),
            PayloadKind::Utf8 => PayloadConfig::Utf8 {
                text: self.utf8_text.clone(),
            },
            PayloadKind::Utf16 => PayloadConfig::Utf16 {
                text: self.utf16_text.clone(),
                byte_order: if self.utf16_big_endian {
                    ByteOrder::BigEndian
                } else {
                    ByteOrder::LittleEndian
                },
                bom: self.utf16_bom,
            },
            PayloadKind::Ascii => PayloadConfig::Ascii {
                text: self.ascii_text.clone(),
                code_page: self.ascii_code_page,
            },
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
        let timestamp = self.timestamp_enabled.then_some(TimestampConfig {
            include_date: self.ts_date,
            include_millis: self.ts_millis,
            include_timezone: self.ts_timezone,
        });
        let checksum = self.checksum_enabled.then_some(ChecksumConfig {
            algorithm: self.checksum_algorithm,
            intentionally_wrong: self.checksum_wrong,
        });
        Some(MessageConfig {
            payload,
            interval_ms,
            timestamp,
            checksum,
        })
    }
}
