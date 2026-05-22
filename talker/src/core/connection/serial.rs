use anyhow::Context;

use super::Connection;
use super::config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};

pub(super) struct SerialConnection {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialConnection {
    pub(super) fn open(config: &SerialConfig) -> anyhow::Result<Self> {
        let port = serialport::new(&config.port, config.baud_rate)
            .data_bits(to_sp_data_bits(config.data_bits))
            .parity(to_sp_parity(config.parity))
            .stop_bits(to_sp_stop_bits(config.stop_bits))
            .flow_control(to_sp_flow_control(config.flow_control))
            .timeout(std::time::Duration::from_secs(1))
            .open()
            .with_context(|| format!("opening serial port {:?}", config.port))?;
        Ok(Self { port })
    }
}

impl Connection for SerialConnection {
    fn send(&mut self, data: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        self.port.write_all(data).context("writing to serial port")
    }
}

fn to_sp_data_bits(d: DataBits) -> serialport::DataBits {
    match d {
        DataBits::Five => serialport::DataBits::Five,
        DataBits::Six => serialport::DataBits::Six,
        DataBits::Seven => serialport::DataBits::Seven,
        DataBits::Eight => serialport::DataBits::Eight,
    }
}

fn to_sp_parity(p: Parity) -> serialport::Parity {
    match p {
        Parity::None => serialport::Parity::None,
        Parity::Odd => serialport::Parity::Odd,
        Parity::Even => serialport::Parity::Even,
    }
}

fn to_sp_stop_bits(s: StopBits) -> serialport::StopBits {
    match s {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

fn to_sp_flow_control(f: FlowControl) -> serialport::FlowControl {
    match f {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Software => serialport::FlowControl::Software,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_nonexistent_port_returns_error() {
        let config = SerialConfig::new("/dev/does_not_exist_xyz");
        let result = SerialConnection::open(&config);
        let msg = result.err().expect("expected error opening nonexistent port").to_string();
        assert!(msg.contains("does_not_exist_xyz"));
    }

    #[test]
    fn data_bits_conversion_covers_all_variants() {
        assert!(matches!(to_sp_data_bits(DataBits::Five), serialport::DataBits::Five));
        assert!(matches!(to_sp_data_bits(DataBits::Six), serialport::DataBits::Six));
        assert!(matches!(to_sp_data_bits(DataBits::Seven), serialport::DataBits::Seven));
        assert!(matches!(to_sp_data_bits(DataBits::Eight), serialport::DataBits::Eight));
    }

    #[test]
    fn parity_conversion_covers_all_variants() {
        assert!(matches!(to_sp_parity(Parity::None), serialport::Parity::None));
        assert!(matches!(to_sp_parity(Parity::Odd), serialport::Parity::Odd));
        assert!(matches!(to_sp_parity(Parity::Even), serialport::Parity::Even));
    }

    #[test]
    fn stop_bits_conversion_covers_all_variants() {
        assert!(matches!(to_sp_stop_bits(StopBits::One), serialport::StopBits::One));
        assert!(matches!(to_sp_stop_bits(StopBits::Two), serialport::StopBits::Two));
    }

    #[test]
    fn flow_control_conversion_covers_all_variants() {
        assert!(matches!(to_sp_flow_control(FlowControl::None), serialport::FlowControl::None));
        assert!(matches!(
            to_sp_flow_control(FlowControl::Software),
            serialport::FlowControl::Software
        ));
        assert!(matches!(
            to_sp_flow_control(FlowControl::Hardware),
            serialport::FlowControl::Hardware
        ));
    }
}
