use std::net::{Ipv4Addr, SocketAddr};

use crate::core::{
    channel::{
        DataBits, FlowControl, InterfaceConfig, Parity, SerialConfig, StopBits, TcpClientConfig,
        UdpConfig, UdpMode,
    },
    message::{
        ByteOrder, ChecksumAlgorithm, ChecksumConfig, CodePage, MessageConfig, NmeaChecksumMode,
        PayloadConfig, TimestampConfig,
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

/// Hold-to-repeat state for a single ± port control. Lives only at runtime;
/// never serialised.
#[derive(Debug, Clone, Copy)]
pub struct PortHold {
    /// −1 for the minus button, +1 for the plus button.
    pub direction: i8,
    /// Seconds until the next repeat fires. Negative while we wait out the
    /// initial delay, then drives the per-frame repeat cadence.
    pub next_fire_in: f32,
    /// Total seconds the button has been held — used to accelerate the
    /// repeat interval (see `port_repeat_interval` in `gui/mod.rs`).
    pub total_held: f32,
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
    pub udp_dest: String,           // unicast destination (host:port)
    pub udp_broadcast_addr: String, // broadcast destination address (defaults to 255.255.255.255)
    pub udp_broadcast_port: String, // broadcast destination port
    /// Transient state for the broadcast port's ± hold-to-repeat. `None`
    /// when nothing is being held; otherwise carries the pressed direction
    /// (−1 or +1), seconds until the next repeat fires (negative = still
    /// waiting), and total seconds held (used to accelerate the rate).
    pub udp_port_hold: Option<PortHold>,
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
            udp_broadcast_addr: Ipv4Addr::BROADCAST.to_string(),
            udp_broadcast_port: String::new(),
            udp_port_hold: None,
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
                let mut draft = Self {
                    kind: ConnKind::Udp,
                    local_port: u.local_port.map(|p| p.to_string()).unwrap_or_default(),
                    ..Default::default()
                };
                match &u.mode {
                    UdpMode::Unicast { destination } => {
                        draft.udp_mode = UdpModeDraft::Unicast;
                        draft.udp_dest = destination.to_string();
                    }
                    UdpMode::Broadcast { destination } => {
                        draft.udp_mode = UdpModeDraft::Broadcast;
                        draft.udp_broadcast_addr = destination.ip().to_string();
                        draft.udp_broadcast_port = destination.port().to_string();
                    }
                    UdpMode::Multicast { group, port, .. } => {
                        draft.udp_mode = UdpModeDraft::Multicast;
                        draft.udp_group = group.to_string();
                        draft.udp_mc_port = port.to_string();
                    }
                }
                draft
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
                    UdpModeDraft::Broadcast => {
                        let addr: Ipv4Addr = self.udp_broadcast_addr.parse().ok()?;
                        let port: u16 = self.udp_broadcast_port.parse().ok()?;
                        UdpConfig::broadcast(SocketAddr::from((addr, port)))
                    }
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
#[derive(Debug, PartialEq, Clone, Copy)]
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
    pub nmea_checksum_mode: NmeaChecksumMode,
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
    /// Scratch buffer for the Insert Byte popup; not part of the message.
    pub insert_byte_hex: String,
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
            nmea_checksum_mode: NmeaChecksumMode::Correct,
            interval_ms: "1000".to_string(),
            timestamp_enabled: false,
            ts_date: true,
            ts_millis: false,
            ts_timezone: true,
            checksum_enabled: false,
            checksum_algorithm: ChecksumAlgorithm::default(),
            checksum_wrong: false,
            insert_byte_hex: String::new(),
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
                nmea_checksum,
            } => {
                d.payload_kind = PayloadKind::Nmea;
                d.nmea_talker = talker.clone();
                d.nmea_sentence_type = sentence_type.clone();
                d.nmea_fields = fields.join(",");
                d.nmea_checksum_mode = *nmea_checksum;
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
                PayloadConfig::Nmea {
                    talker: self.nmea_talker.clone(),
                    sentence_type: self.nmea_sentence_type.clone(),
                    fields,
                    nmea_checksum: self.nmea_checksum_mode,
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── ConnDraft round-trips ─────────────────────────────────────────────────
    //
    // A GUI-representable interface config must survive
    // InterfaceConfig -> ConnDraft -> to_config() unchanged.

    fn conn_round_trip(cfg: InterfaceConfig) {
        let draft = ConnDraft::from(&cfg);
        assert_eq!(draft.to_config(), Some(cfg));
    }

    #[test]
    fn serial_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Serial(SerialConfig {
            port: "COM7".to_string(),
            baud_rate: 38400,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::Hardware,
        }));
    }

    #[test]
    fn udp_unicast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::unicast(
            "192.168.1.50:4000".parse().unwrap(),
        )));
    }

    #[test]
    fn udp_broadcast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::broadcast(
            "255.255.255.255:9000".parse().unwrap(),
        )));
    }

    #[test]
    fn udp_multicast_draft_round_trip() {
        conn_round_trip(InterfaceConfig::Udp(UdpConfig::multicast(
            "239.0.0.7".parse().unwrap(),
            5500,
        )));
    }

    #[test]
    fn udp_draft_preserves_local_port() {
        let mut udp = UdpConfig::unicast("127.0.0.1:5000".parse().unwrap());
        udp.local_port = Some(6000);
        conn_round_trip(InterfaceConfig::Udp(udp));
    }

    #[test]
    fn tcp_draft_round_trip() {
        conn_round_trip(InterfaceConfig::TcpClient(TcpClientConfig::new(
            "10.0.0.1:4001".parse().unwrap(),
        )));
    }

    // ── ScheduleDraft round-trips ─────────────────────────────────────────────
    //
    // A message must survive MessageConfig -> ScheduleDraft -> to_message_config()
    // unchanged, for every payload format and with/without timestamp + checksum.

    fn message_round_trip(m: MessageConfig) {
        let draft = ScheduleDraft::from(&m);
        assert_eq!(draft.to_message_config(), Some(m));
    }

    #[test]
    fn hex_message_round_trip() {
        message_round_trip(MessageConfig::new(PayloadConfig::raw_hex("DEADBEEF"), 500));
    }

    #[test]
    fn utf8_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Utf8 {
                text: "héllo".to_string(),
            },
            1000,
        ));
    }

    #[test]
    fn utf16_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Utf16 {
                text: "data".to_string(),
                byte_order: ByteOrder::LittleEndian,
                bom: true,
            },
            250,
        ));
    }

    #[test]
    fn ascii_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::Ascii {
                text: "café".to_string(),
                code_page: CodePage::Cp437,
            },
            750,
        ));
    }

    #[test]
    fn nmea_message_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::nmea("GP", "GGA", vec!["123519".to_string(), "N".to_string()]),
            1000,
        ));
    }

    #[test]
    fn nmea_message_with_no_fields_round_trip() {
        message_round_trip(MessageConfig::new(
            PayloadConfig::nmea("GN", "RMC", vec![]),
            2000,
        ));
    }

    #[test]
    fn message_round_trip_with_timestamp_and_checksum() {
        message_round_trip(MessageConfig {
            payload: PayloadConfig::raw_hex("AABB"),
            interval_ms: 1000,
            timestamp: Some(TimestampConfig {
                include_date: true,
                include_millis: false,
                include_timezone: true,
            }),
            checksum: Some(ChecksumConfig {
                algorithm: ChecksumAlgorithm::Crc16Modbus,
                intentionally_wrong: true,
            }),
        });
    }

    #[test]
    fn message_without_timestamp_or_checksum_stays_disabled() {
        let m = MessageConfig::new(PayloadConfig::raw_hex("00"), 100);
        let draft = ScheduleDraft::from(&m);
        assert!(!draft.timestamp_enabled);
        assert!(!draft.checksum_enabled);
        assert_eq!(draft.to_message_config(), Some(m));
    }

    #[test]
    fn switching_format_does_not_lose_other_buffers() {
        // A hex message loaded as a draft keeps usable defaults in the other
        // format buffers, so toggling the selector never panics or blanks out.
        let draft = ScheduleDraft::from(&MessageConfig::new(PayloadConfig::raw_hex("FF"), 100));
        assert_eq!(draft.payload_kind, PayloadKind::Hex);
        assert_eq!(draft.nmea_talker, "GP");
    }
}
