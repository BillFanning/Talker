use std::time::{Duration, Instant};

use crate::core::{
    channel::{Interface, InterfaceConfig},
    scheduler::Schedule,
};

/// A command sent from the UI thread to a talker thread.
pub enum TalkerCommand {
    Stop,
    /// Reopen the channel's interface with a new configuration.
    UpdateInterface(InterfaceConfig),
}

/// A status update sent from a talker thread to the UI thread.
pub enum TalkerStatus {
    SendCount(u64),
    ConnectionError { message: String },
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
/// every 50 ms so the loop stays responsive even on long send intervals.
pub fn run_talker(
    mut interface: Box<dyn Interface>,
    mut schedule: Schedule,
    cmd_rx: crossbeam_channel::Receiver<TalkerCommand>,
    status_tx: crossbeam_channel::Sender<TalkerStatus>,
) {
    let mut sent_count = 0u64;
    let mut next_send = Instant::now();

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
            }
        }

        if Instant::now() >= next_send {
            let (payload, interval) = {
                let entry = schedule.next_entry();
                (entry.payload.clone(), entry.interval)
            };
            match interface.send(&payload) {
                Ok(()) => {
                    sent_count += 1;
                    let _ = status_tx.try_send(TalkerStatus::SendCount(sent_count));
                }
                Err(e) => {
                    tracing::warn!("send failed: {e:#}");
                    let _ = status_tx.try_send(TalkerStatus::ConnectionError {
                        message: format!("{e:#}"),
                    });
                }
            }
            next_send = Instant::now() + interval;
        }

        // Sleep until the next send is due, capped so commands are still
        // polled promptly.
        let wait = next_send.saturating_duration_since(Instant::now());
        std::thread::sleep(wait.min(Duration::from_millis(50)));
    }
}
