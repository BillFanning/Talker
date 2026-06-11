//! Map validated configuration to live runtime objects (spec §128 — the runtime
//! owns this mapping, not config).
//!
//! Pure and synchronous: it constructs extractors and *unopened*
//! transports from a [`ChannelConfig`](crate::config::ChannelConfig). Opening /
//! binding is async and happens at Start in the orchestrator (§8.2, §71). Serial
//! parameters that `serialport` cannot represent (Mark/Space parity, 1.5 stop
//! bits — §80.1) are rejected here rather than silently downgraded.

use std::net::SocketAddr;

use serialport::{DataBits, FlowControl, Parity, StopBits};

use crate::config::schema::{
    DataBits as CfgDataBits, DisplayViewConfig, ExtractionConfig, FlowControl as CfgFlowControl,
    Parity as CfgParity, SerialConfig, StopBits as CfgStopBits, TcpListenerConfig, UdpConfig,
};
use crate::core::ChannelId;
use crate::display::DisplayView;
use crate::extract::{DelimiterExtractor, FixedLengthExtractor, MessageExtractor, StreamExtractor};
use crate::transport::serial::SerialTransport;
use crate::transport::tcp::TcpListenerTransport;
use crate::transport::udp::UdpTransport;

/// A configuration could not be realized as a runtime object.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("serial parity Mark/Space is not supported by the serial backend")]
    UnsupportedParity,
    #[error("1.5 stop bits is not supported by the serial backend")]
    UnsupportedStopBits,
    #[error("invalid socket address {address:?}:{port}")]
    InvalidSocketAddr { address: String, port: u16 },
    #[error("invalid multicast group address {0:?}")]
    InvalidMulticastGroup(String),
    #[error("invalid multicast interface address {0:?}")]
    InvalidMulticastInterface(String),
}

/// Build the extractor for a Channel (§20).
pub fn build_extractor(config: &ExtractionConfig) -> Box<dyn MessageExtractor + Send> {
    match config {
        ExtractionConfig::Stream => Box::new(StreamExtractor::new()),
        ExtractionConfig::Delimiter {
            delimiter,
            include_delimiter,
        } => Box::new(DelimiterExtractor::new(
            delimiter.clone(),
            *include_delimiter,
        )),
        ExtractionConfig::FixedLength {
            length,
            sync_marker,
        } => Box::new(FixedLengthExtractor::new(*length, sync_marker.clone())),
    }
}

/// Build a Display View renderer from its config (§47, §78). Visual-only fields
/// (font, colors) don't affect produced text; the actual wrap width is set by
/// the UI at render time, so it starts unset here.
pub fn build_display_view(config: &DisplayViewConfig) -> DisplayView {
    DisplayView {
        mode: config.mode,
        encoding: config.encoding,
        character_rendering: config.character_rendering,
        wrapping: config.wrapping,
        wrap_width: None,
        hex_separator: " ".to_string(),
        hex_bytes_per_line: 16,
    }
}

fn map_data_bits(bits: CfgDataBits) -> DataBits {
    match bits {
        CfgDataBits::Five => DataBits::Five,
        CfgDataBits::Six => DataBits::Six,
        CfgDataBits::Seven => DataBits::Seven,
        CfgDataBits::Eight => DataBits::Eight,
    }
}

fn map_parity(parity: CfgParity) -> Result<Parity, BuildError> {
    match parity {
        CfgParity::None => Ok(Parity::None),
        CfgParity::Even => Ok(Parity::Even),
        CfgParity::Odd => Ok(Parity::Odd),
        CfgParity::Mark | CfgParity::Space => Err(BuildError::UnsupportedParity),
    }
}

fn map_stop_bits(bits: CfgStopBits) -> Result<StopBits, BuildError> {
    match bits {
        CfgStopBits::One => Ok(StopBits::One),
        CfgStopBits::Two => Ok(StopBits::Two),
        CfgStopBits::OnePointFive => Err(BuildError::UnsupportedStopBits),
    }
}

fn map_flow_control(flow: CfgFlowControl) -> FlowControl {
    match flow {
        CfgFlowControl::None => FlowControl::None,
        CfgFlowControl::XonXoff => FlowControl::Software,
        CfgFlowControl::RtsCts => FlowControl::Hardware,
    }
}

/// Build an unopened serial transport (§14, §74).
pub fn build_serial(
    channel_id: ChannelId,
    config: &SerialConfig,
) -> Result<SerialTransport, BuildError> {
    let mut transport = SerialTransport::new(channel_id, config.port.clone(), config.baud_rate)
        .with_data_bits(map_data_bits(config.data_bits))
        .with_parity(map_parity(config.parity)?)
        .with_stop_bits(map_stop_bits(config.stop_bits)?)
        .with_flow_control(map_flow_control(config.flow_control));
    if let Some(rts) = config.rts {
        transport = transport.with_rts(rts);
    }
    if let Some(dtr) = config.dtr {
        transport = transport.with_dtr(dtr);
    }
    Ok(transport)
}

fn socket_addr(address: &str, port: u16) -> Result<SocketAddr, BuildError> {
    format!("{address}:{port}")
        .parse()
        .map_err(|_| BuildError::InvalidSocketAddr {
            address: address.to_string(),
            port,
        })
}

/// Build an unbound UDP transport (§15, §75).
pub fn build_udp(channel_id: ChannelId, config: &UdpConfig) -> Result<UdpTransport, BuildError> {
    let addr = socket_addr(&config.bind_address, config.port)?;
    let mut transport = UdpTransport::new(channel_id, addr, config.mode);
    if let Some(group) = &config.multicast_group {
        let ip = group
            .parse()
            .map_err(|_| BuildError::InvalidMulticastGroup(group.clone()))?;
        transport = transport.with_multicast_group(ip);
    }
    if let Some(iface) = &config.multicast_interface {
        let ip = iface
            .parse()
            .map_err(|_| BuildError::InvalidMulticastInterface(iface.clone()))?;
        transport = transport.with_multicast_interface(ip);
    }
    if let Some(bytes) = config.recv_buffer_bytes {
        transport = transport.with_recv_buffer(bytes);
    }
    Ok(transport)
}

/// Build an unbound TCP listener transport (§16, §76).
pub fn build_tcp_listener(
    channel_id: ChannelId,
    config: &TcpListenerConfig,
) -> Result<TcpListenerTransport, BuildError> {
    let addr = socket_addr(&config.bind_address, config.port)?;
    Ok(TcpListenerTransport::new(channel_id, addr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChannelId, ChunkTime};

    #[test]
    fn stream_extractor_emits_nothing() {
        let mut ex = build_extractor(&ExtractionConfig::Stream);
        assert!(ex.push_chunk(b"anything", ChunkTime::now()).is_empty());
    }

    #[test]
    fn unsupported_serial_parameters_are_rejected() {
        assert!(matches!(
            map_parity(CfgParity::Mark),
            Err(BuildError::UnsupportedParity)
        ));
        assert!(matches!(
            map_parity(CfgParity::Space),
            Err(BuildError::UnsupportedParity)
        ));
        assert!(matches!(
            map_stop_bits(CfgStopBits::OnePointFive),
            Err(BuildError::UnsupportedStopBits)
        ));
        // Supported ones map cleanly.
        assert!(map_parity(CfgParity::Even).is_ok());
        assert!(map_stop_bits(CfgStopBits::Two).is_ok());
    }

    #[test]
    fn udp_address_is_validated() {
        let good = UdpConfig {
            bind_address: "127.0.0.1".to_string(),
            port: 9000,
            mode: crate::transport::udp::UdpMode::Unicast,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer_bytes: None,
        };
        assert!(build_udp(ChannelId::new(), &good).is_ok());

        let bad = UdpConfig {
            bind_address: "not-an-address".to_string(),
            port: 9000,
            mode: crate::transport::udp::UdpMode::Unicast,
            multicast_group: None,
            multicast_interface: None,
            recv_buffer_bytes: None,
        };
        assert!(matches!(
            build_udp(ChannelId::new(), &bad),
            Err(BuildError::InvalidSocketAddr { .. })
        ));
    }
}
