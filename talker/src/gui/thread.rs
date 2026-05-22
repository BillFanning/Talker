use std::time::{Duration, Instant};

use crate::core::{
    connection::{ConnectionCollection, ConnectionConfig},
    scheduler::Schedule,
};

pub enum TalkerCommand {
    Stop,
    UpdateConnection(usize, ConnectionConfig),
}

pub enum TalkerStatus {
    SendCount(u64),
    ConnectionError { message: String },
}

pub struct TalkerHandle {
    pub cmd_tx: crossbeam_channel::Sender<TalkerCommand>,
    pub status_rx: crossbeam_channel::Receiver<TalkerStatus>,
    pub thread: std::thread::JoinHandle<()>,
}

pub fn run_talker(
    mut connections: ConnectionCollection,
    mut schedule: Schedule,
    cmd_rx: crossbeam_channel::Receiver<TalkerCommand>,
    status_tx: crossbeam_channel::Sender<TalkerStatus>,
) {
    let mut sent_count = 0u64;
    loop {
        for cmd in cmd_rx.try_iter() {
            if !handle_cmd(cmd, &mut connections, &status_tx) {
                return;
            }
        }

        let (payload, interval) = {
            let e = schedule.next_entry();
            (e.payload.clone(), e.interval)
        };

        let errors = connections.send_reporting(&payload);
        if errors.is_empty() {
            sent_count += 1;
            let _ = status_tx.try_send(TalkerStatus::SendCount(sent_count));
        } else {
            for (i, msg) in errors {
                tracing::warn!("connection {i} send failed: {msg}");
                let _ = status_tx.try_send(TalkerStatus::ConnectionError { message: msg });
            }
        }

        let deadline = Instant::now() + interval;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
            for cmd in cmd_rx.try_iter() {
                if !handle_cmd(cmd, &mut connections, &status_tx) {
                    return;
                }
            }
        }
    }
}

fn handle_cmd(
    cmd: TalkerCommand,
    connections: &mut ConnectionCollection,
    status_tx: &crossbeam_channel::Sender<TalkerStatus>,
) -> bool {
    match cmd {
        TalkerCommand::Stop => false,
        TalkerCommand::UpdateConnection(i, cfg) => {
            match cfg.open() {
                Ok(conn) => {
                    connections.replace(i, conn);
                    tracing::info!("connection {i} updated");
                }
                Err(e) => {
                    tracing::warn!("connection {i} update failed: {e:#}");
                    let _ = status_tx.try_send(TalkerStatus::ConnectionError {
                        message: format!("{e:#}"),
                    });
                }
            }
            true
        }
    }
}
