use std::time::{Duration, Instant};

use crate::core::{
    channel::{Interface, InterfaceConfig},
    scheduler::{Schedule, Tick},
};

/// A command sent from the UI thread to a talker thread.
pub enum TalkerCommand {
    Stop,
    /// Reopen the channel's interface with a new configuration.
    UpdateInterface(InterfaceConfig),
    /// Change message `index`'s send interval, effective immediately.
    SetInterval {
        index: usize,
        interval_ms: u64,
    },
}

/// A status update sent from a talker thread to the UI thread.
pub enum TalkerStatus {
    /// A message was sent — the running count and the bytes put on the wire.
    Sent {
        count: u64,
        payload: Vec<u8>,
    },
    ConnectionError {
        message: String,
    },
}

/// The UI-side handle for a running talker thread.
pub struct TalkerHandle {
    pub cmd_tx: crossbeam_channel::Sender<TalkerCommand>,
    pub status_rx: crossbeam_channel::Receiver<TalkerStatus>,
    pub thread: std::thread::JoinHandle<()>,
}

/// Run one channel's talker loop until [`TalkerCommand::Stop`] is received.
///
/// Owns a single interface and the channel's schedule. Commands are polled
/// before each schedule tick and after every wait, so the loop stays
/// responsive even on long send intervals.
pub fn run_talker(
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    cmd_rx: crossbeam_channel::Receiver<TalkerCommand>,
    status_tx: crossbeam_channel::Sender<TalkerStatus>,
) {
    let mut sent_count = 0u64;

    loop {
        for cmd in cmd_rx.try_iter() {
            match cmd {
                TalkerCommand::Stop => return,
                TalkerCommand::UpdateInterface(cfg) => match cfg.open() {
                    Ok(new) => {
                        interface = new;
                        tracing::info!("channel interface updated");
                    }
                    Err(e) => {
                        tracing::warn!("interface update failed: {e:#}");
                        let _ = status_tx.try_send(TalkerStatus::ConnectionError {
                            message: format!("{e:#}"),
                        });
                    }
                },
                TalkerCommand::SetInterval { index, interval_ms } => {
                    schedule.set_interval(index, interval_ms, Instant::now());
                }
            }
        }

        match schedule.poll(Instant::now()) {
            Tick::Send { payload, .. } => match interface.send(&payload) {
                Ok(()) => {
                    sent_count += 1;
                    let _ = status_tx.try_send(TalkerStatus::Sent {
                        count: sent_count,
                        payload,
                    });
                }
                Err(e) => {
                    tracing::warn!("send failed: {e:#}");
                    let _ = status_tx.try_send(TalkerStatus::ConnectionError {
                        message: format!("{e:#}"),
                    });
                }
            },
            Tick::Wait(until) => {
                let remaining = until.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(50)));
            }
            Tick::Idle => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}
