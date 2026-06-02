//! Default channel templates (spec §81–§85).
//!
//! Templates provide safe, valid, *stopped* starting configurations (§81). They
//! never open interfaces. Resource-bearing fields (serial port name, UDP/TCP
//! port) are placeholders the user fills in before Start (§71).

use crate::core::{ChannelKind, ChannelName};
use crate::decode::NmeaValidationMode;
use crate::display::{CharacterRendering, DisplayEncoding, DisplayMode, WrappingMode};
use crate::transport::udp::UdpMode;

use super::schema::*;

fn view(mode: DisplayMode, metadata_visible: bool) -> DisplayViewConfig {
    DisplayViewConfig {
        mode,
        encoding: DisplayEncoding::Utf8,
        character_rendering: CharacterRendering::Native,
        font: None,
        foreground_color: None,
        background_color: None,
        wrapping: WrappingMode::NoWrap,
        metadata_visible,
    }
}

fn raw_and_hex() -> DisplayConfig {
    DisplayConfig {
        views: vec![view(DisplayMode::Raw, false), view(DisplayMode::Hex, false)],
    }
}

fn raw_with_metadata() -> DisplayConfig {
    DisplayConfig {
        views: vec![view(DisplayMode::Raw, true)],
    }
}

/// A bounded default retention so templates validate (§80).
fn default_retention() -> RetentionConfig {
    RetentionConfig::with_message_limit(10_000)
}

/// Generic serial channel (§82): 9600 8N1, Stream, no decoder, Raw + Hex.
pub fn serial_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("Serial Channel"),
        kind: ChannelKind::Serial,
        interface: InterfaceConfig::Serial(SerialConfig {
            port: String::new(),
            baud_rate: 9600,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            rts: None,
            dtr: None,
        }),
        extraction: ExtractionConfig::Stream,
        decoder: DecoderConfig::None,
        display: raw_and_hex(),
        recording: RecordingConfig::default(),
        retention: default_retention(),
    }
}

/// NMEA serial channel (§83): 4800 8N1, CRLF delimiter (excluded), NMEA decoder
/// in Standard mode, Raw + Metadata.
pub fn nmea_serial_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("NMEA Serial"),
        kind: ChannelKind::Serial,
        interface: InterfaceConfig::Serial(SerialConfig {
            port: String::new(),
            baud_rate: 4800,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            rts: None,
            dtr: None,
        }),
        extraction: ExtractionConfig::Delimiter {
            delimiter: vec![b'\r', b'\n'],
            include_delimiter: false,
        },
        decoder: DecoderConfig::Nmea0183 {
            validation_mode: NmeaValidationMode::Standard,
        },
        display: raw_with_metadata(),
        recording: RecordingConfig::default(),
        retention: default_retention(),
    }
}

/// UDP channel (§84): bind 0.0.0.0, unicast, each datagram a Message, Raw + Hex.
pub fn udp_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("UDP Channel"),
        kind: ChannelKind::Udp,
        interface: InterfaceConfig::Udp(UdpConfig {
            bind_address: "0.0.0.0".to_string(),
            port: 0,
            mode: UdpMode::Unicast,
            multicast_group: None,
        }),
        extraction: ExtractionConfig::Stream,
        decoder: DecoderConfig::None,
        display: raw_and_hex(),
        recording: RecordingConfig::default(),
        retention: default_retention(),
    }
}

/// TCP listener (§85): bind 0.0.0.0, no connection cap. Extraction/decoder/display
/// here apply to its accepted connection channels (§16.2).
pub fn tcp_listener_template() -> ChannelConfig {
    ChannelConfig {
        id: None,
        name: ChannelName::new("TCP Listener"),
        kind: ChannelKind::TcpListener,
        interface: InterfaceConfig::TcpListener(TcpListenerConfig {
            bind_address: "0.0.0.0".to_string(),
            port: 0,
            max_connections: None,
        }),
        extraction: ExtractionConfig::Stream,
        decoder: DecoderConfig::None,
        display: raw_and_hex(),
        recording: RecordingConfig::default(),
        retention: default_retention(),
    }
}
