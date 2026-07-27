//! Static wire-demand and measured application-service capacity estimates.
//!
//! Static serial math can prove that a requested sustained byte rate exceeds
//! the configured UART line rate. Measured render/send estimates are advisory:
//! they describe observed application call time, not physical transmission or
//! a hard real-time guarantee.

use std::time::Duration;

use super::{
    channel::{DataBits, Parity, SerialConfig, StopBits},
    telemetry::SendTimingTelemetry,
};

/// Recent render/send samples required before presenting a p99 estimate.
pub const MIN_SERVICE_SAMPLES: u64 = 20;

/// One compiled message's scheduled wire demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageDemand {
    pub wire_bytes: usize,
    pub interval_ms: u64,
}

impl MessageDemand {
    pub const fn new(wire_bytes: usize, interval_ms: u64) -> Self {
        Self {
            wire_bytes,
            interval_ms,
        }
    }
}

/// Aggregate requested cadence for one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelDemand {
    pub active_messages: usize,
    pub messages_per_second: f64,
    pub bytes_per_second: f64,
    pub shortest_interval: Option<Duration>,
}

impl ChannelDemand {
    pub fn from_messages(messages: impl IntoIterator<Item = MessageDemand>) -> Self {
        let mut demand = Self::default();
        for message in messages {
            demand.include(message);
        }
        demand
    }

    /// Add one message without allocating an intermediate collection. GUI
    /// callers use this to fold already-memoized message analyses per frame.
    pub fn include(&mut self, message: MessageDemand) {
        if message.interval_ms == 0 {
            return;
        }
        let interval = Duration::from_millis(message.interval_ms);
        let messages_per_second = 1_000.0 / message.interval_ms as f64;
        self.active_messages += 1;
        self.messages_per_second += messages_per_second;
        self.bytes_per_second += message.wire_bytes as f64 * messages_per_second;
        self.shortest_interval = Some(
            self.shortest_interval
                .map_or(interval, |shortest| shortest.min(interval)),
        );
    }

    pub fn is_active(self) -> bool {
        self.active_messages > 0
    }
}

/// Physical UART line-rate estimate for a channel's aggregate demand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SerialLineEstimate {
    pub bits_per_wire_byte: u8,
    pub required_bits_per_second: f64,
    pub available_bits_per_second: f64,
    pub utilization: f64,
}

impl SerialLineEstimate {
    pub fn is_oversubscribed(self) -> bool {
        self.required_bits_per_second > self.available_bits_per_second
    }

    pub fn headroom_factor(self) -> Option<f64> {
        (self.required_bits_per_second > 0.0)
            .then(|| self.available_bits_per_second / self.required_bits_per_second)
    }

    pub fn minimum_baud(self) -> u64 {
        self.required_bits_per_second.ceil() as u64
    }
}

/// Calculate sustained physical serial demand. Returns `None` for an invalid
/// zero-baud configuration; config validation reports that separately.
pub fn serial_line_estimate(
    demand: ChannelDemand,
    config: &SerialConfig,
) -> Option<SerialLineEstimate> {
    if config.baud_rate == 0 {
        return None;
    }
    let bits_per_wire_byte =
        1 + data_bits(config.data_bits) + parity_bits(config.parity) + stop_bits(config.stop_bits);
    let required_bits_per_second = demand.bytes_per_second * f64::from(bits_per_wire_byte);
    let available_bits_per_second = f64::from(config.baud_rate);
    Some(SerialLineEstimate {
        bits_per_wire_byte,
        required_bits_per_second,
        available_bits_per_second,
        utilization: required_bits_per_second / available_bits_per_second,
    })
}

const fn data_bits(bits: DataBits) -> u8 {
    match bits {
        DataBits::Five => 5,
        DataBits::Six => 6,
        DataBits::Seven => 7,
        DataBits::Eight => 8,
    }
}

const fn parity_bits(parity: Parity) -> u8 {
    match parity {
        Parity::None => 0,
        Parity::Odd | Parity::Even => 1,
    }
}

const fn stop_bits(bits: StopBits) -> u8 {
    match bits {
        StopBits::One => 1,
        StopBits::Two => 2,
    }
}

/// Advisory application-service estimate formed by adding the separate render
/// and synchronous-send p99 histogram upper bounds. The sum is not a joint p99.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ServiceEstimate {
    pub samples: u64,
    pub summed_p99_upper_bounds: Duration,
    pub capacity_messages_per_second: f64,
    pub utilization: f64,
}

impl ServiceEstimate {
    /// Estimated service capacity divided by requested demand.
    ///
    /// A zero utilization has unbounded headroom. Positive infinity has no
    /// headroom; invalid negative or NaN values, which can only arise when a
    /// caller constructs this public value directly, also fail conservatively
    /// to zero headroom instead of leaking a misleading negative/NaN readout.
    pub fn headroom_factor(self) -> f64 {
        if self.utilization == 0.0 {
            f64::INFINITY
        } else if !self.utilization.is_finite() || self.utilization < 0.0 {
            0.0
        } else {
            self.utilization.recip()
        }
    }
}

pub fn service_sample_count(timing: SendTimingTelemetry) -> u64 {
    timing
        .render_duration
        .sample_count()
        .min(timing.send_duration.sample_count())
}

/// Estimate application-side throughput from recent timing. Returns `None`
/// until there is active demand and enough paired render/send samples.
pub fn measured_service_estimate(
    demand: ChannelDemand,
    timing: SendTimingTelemetry,
) -> Option<ServiceEstimate> {
    if !demand.is_active() {
        return None;
    }
    let samples = service_sample_count(timing);
    if samples < MIN_SERVICE_SAMPLES {
        return None;
    }
    let render = timing.render_duration.percentile_upper_bound(99)?;
    let send = timing.send_duration.percentile_upper_bound(99)?;
    let summed_p99_upper_bounds = render.saturating_add(send);
    let service_seconds = summed_p99_upper_bounds.as_secs_f64();
    if service_seconds == 0.0 {
        return None;
    }
    let capacity_messages_per_second = 1.0 / service_seconds;
    Some(ServiceEstimate {
        samples,
        summed_p99_upper_bounds,
        capacity_messages_per_second,
        utilization: demand.messages_per_second / capacity_messages_per_second,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serial(baud_rate: u32) -> SerialConfig {
        let mut config = SerialConfig::new("COM1");
        config.baud_rate = baud_rate;
        config
    }

    #[test]
    fn channel_demand_sums_active_rates_and_ignores_dormant_messages() {
        let demand = ChannelDemand::from_messages([
            MessageDemand::new(100, 1_000),
            MessageDemand::new(50, 500),
            MessageDemand::new(1_000_000, 0),
        ]);

        assert_eq!(demand.active_messages, 2);
        assert_eq!(demand.messages_per_second, 3.0);
        assert_eq!(demand.bytes_per_second, 200.0);
        assert_eq!(demand.shortest_interval, Some(Duration::from_millis(500)));
    }

    #[test]
    fn uart_estimate_uses_start_data_parity_and_stop_bits() {
        let demand = ChannelDemand::from_messages([MessageDemand::new(100, 100)]);
        let eight_n_one = serial_line_estimate(demand, &serial(9_600)).unwrap();
        assert_eq!(eight_n_one.bits_per_wire_byte, 10);
        assert_eq!(eight_n_one.required_bits_per_second, 10_000.0);
        assert!(eight_n_one.is_oversubscribed());
        assert_eq!(eight_n_one.minimum_baud(), 10_000);

        let mut seven_e_two_config = serial(11_000);
        seven_e_two_config.data_bits = DataBits::Seven;
        seven_e_two_config.parity = Parity::Even;
        seven_e_two_config.stop_bits = StopBits::Two;
        let seven_e_two = serial_line_estimate(demand, &seven_e_two_config).unwrap();
        assert_eq!(seven_e_two.bits_per_wire_byte, 11);
        assert_eq!(seven_e_two.required_bits_per_second, 11_000.0);
        assert!(!seven_e_two.is_oversubscribed());
        assert_eq!(seven_e_two.headroom_factor(), Some(1.0));
    }

    #[test]
    fn zero_baud_has_no_line_estimate() {
        let demand = ChannelDemand::from_messages([MessageDemand::new(10, 1_000)]);
        assert_eq!(serial_line_estimate(demand, &serial(0)), None);
    }

    #[test]
    fn service_estimate_waits_for_twenty_paired_samples() {
        let demand = ChannelDemand::from_messages([MessageDemand::new(10, 1)]);
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..MIN_SERVICE_SAMPLES - 1 {
            timing.render_duration.record(Duration::from_micros(100));
            timing.send_duration.record(Duration::from_micros(400));
        }
        assert_eq!(service_sample_count(timing), MIN_SERVICE_SAMPLES - 1);
        assert_eq!(measured_service_estimate(demand, timing), None);

        timing.render_duration.record(Duration::from_micros(100));
        timing.send_duration.record(Duration::from_micros(400));
        let estimate = measured_service_estimate(demand, timing).unwrap();
        // Histogram upper bounds are 100 us + 500 us, not the raw 400 us.
        assert_eq!(estimate.summed_p99_upper_bounds, Duration::from_micros(600));
        assert!((estimate.capacity_messages_per_second - 1_666.666_666).abs() < 0.001);
        assert!((estimate.utilization - 0.6).abs() < f64::EPSILON * 4.0);
        assert!((estimate.headroom_factor() - 1.666_666_666).abs() < 0.001);
    }

    #[test]
    fn service_headroom_has_defined_results_for_public_edge_values() {
        let with_utilization = |utilization| ServiceEstimate {
            samples: MIN_SERVICE_SAMPLES,
            summed_p99_upper_bounds: Duration::from_millis(1),
            capacity_messages_per_second: 1_000.0,
            utilization,
        };

        assert_eq!(with_utilization(0.0).headroom_factor(), f64::INFINITY);
        assert_eq!(with_utilization(f64::INFINITY).headroom_factor(), 0.0);
        assert_eq!(with_utilization(f64::NEG_INFINITY).headroom_factor(), 0.0);
        assert_eq!(with_utilization(-1.0).headroom_factor(), 0.0);
        assert_eq!(with_utilization(f64::NAN).headroom_factor(), 0.0);
    }

    #[test]
    fn dormant_schedule_has_no_measured_service_estimate() {
        let demand = ChannelDemand::from_messages([MessageDemand::new(10, 0)]);
        let mut timing = SendTimingTelemetry::default();
        for _ in 0..MIN_SERVICE_SAMPLES {
            timing.render_duration.record(Duration::from_micros(100));
            timing.send_duration.record(Duration::from_micros(100));
        }
        assert_eq!(measured_service_estimate(demand, timing), None);
    }
}
