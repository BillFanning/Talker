//! Serial transport (spec §14).
//!
//! Unlike UDP/TCP, a serial port has no async-native API, so its continuous
//! blocking receive loop runs on a **dedicated OS thread** that owns the port
//! handle (ADR-001 / §97.1). That thread hands data to the async runtime via
//! `Sender::blocking_send` — the one edge permitted to stall the reader (§97.2,
//! §99): a full extractor queue backpressures the read loop, which can cause a
//! UART/driver overrun reported as transport-specific loss (§101).
//!
//! Cancellation is cooperative (§111): the port is opened with a bounded read
//! timeout, so the loop periodically returns from a blocking read to observe the
//! [`CancellationToken`] — shutdown never relies on interrupting an in-progress
//! read. Completion is signalled through a `oneshot`, so awaiting the thread
//! never blocks a runtime worker ([`TransportJoinHandle::Thread`], §138).
//!
//! Opening the port is a bounded blocking operation and runs on `spawn_blocking`
//! (§97.1); it is fallible so resource errors surface at Channel Start (§71).
//! The continuous read loop is generic over a small [`BlockingReader`] seam so
//! its logic is unit-testable without serial hardware.

use std::io::{self, Read};
use std::time::Duration;

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, ChunkTime};

use super::{
    DataTransportRunner, ReceivedData, ReceivedPayload, TransportJoinHandle, TransportOutcome,
};

/// Read buffer size for one blocking read (a serial chunk; framing is the
/// extractor's job, §105).
const READ_BUFFER: usize = 4096;

/// Default bounded read timeout. Caps how long a blocking read parks before the
/// loop re-checks cancellation (§111).
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// An unopened serial transport description (§14, §74). Call
/// [`open`](Self::open) at Channel Start to acquire the port.
///
/// Parameters use `serialport`'s native enums. The profile schema's richer
/// `Parity`/`StopBits` variants (§80.1: Mark/Space parity, 1.5 stop bits) that
/// `serialport` cannot represent are rejected when config maps to this type.
#[derive(Clone, Debug)]
pub struct SerialTransport {
    channel_id: ChannelId,
    port: String,
    baud_rate: u32,
    data_bits: DataBits,
    parity: Parity,
    stop_bits: StopBits,
    flow_control: FlowControl,
    rts: Option<bool>,
    dtr: Option<bool>,
    read_timeout: Duration,
}

impl SerialTransport {
    /// A serial transport at 8N1, no flow control — the §82 template defaults.
    pub fn new(channel_id: ChannelId, port: impl Into<String>, baud_rate: u32) -> Self {
        Self {
            channel_id,
            port: port.into(),
            baud_rate,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            rts: None,
            dtr: None,
            read_timeout: DEFAULT_READ_TIMEOUT,
        }
    }

    pub fn with_data_bits(mut self, data_bits: DataBits) -> Self {
        self.data_bits = data_bits;
        self
    }

    pub fn with_parity(mut self, parity: Parity) -> Self {
        self.parity = parity;
        self
    }

    pub fn with_stop_bits(mut self, stop_bits: StopBits) -> Self {
        self.stop_bits = stop_bits;
        self
    }

    pub fn with_flow_control(mut self, flow_control: FlowControl) -> Self {
        self.flow_control = flow_control;
        self
    }

    /// Set the initial RTS line state (§14.2). `None` leaves it OS-default.
    pub fn with_rts(mut self, rts: bool) -> Self {
        self.rts = Some(rts);
        self
    }

    /// Set the initial DTR line state (§14.2).
    pub fn with_dtr(mut self, dtr: bool) -> Self {
        self.dtr = Some(dtr);
        self
    }

    pub fn with_read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    /// Open the port (§8.2). A bounded blocking op, run on `spawn_blocking`
    /// (§97.1). Fallible so the runtime can take Starting → Faulted (§9, §71).
    pub async fn open(self) -> serialport::Result<OpenSerialTransport> {
        tokio::task::spawn_blocking(move || self.open_blocking())
            .await
            .expect("serial open task should not panic")
    }

    fn open_blocking(self) -> serialport::Result<OpenSerialTransport> {
        let mut port = serialport::new(&self.port, self.baud_rate)
            .data_bits(self.data_bits)
            .parity(self.parity)
            .stop_bits(self.stop_bits)
            .flow_control(self.flow_control)
            .timeout(self.read_timeout)
            .open()?;
        if let Some(rts) = self.rts {
            port.write_request_to_send(rts)?;
        }
        if let Some(dtr) = self.dtr {
            port.write_data_terminal_ready(dtr)?;
        }
        Ok(OpenSerialTransport {
            channel_id: self.channel_id,
            port,
        })
    }
}

/// An opened serial port ready to receive. Implements [`DataTransportRunner`].
pub struct OpenSerialTransport {
    channel_id: ChannelId,
    port: Box<dyn SerialPort>,
}

impl OpenSerialTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }
}

impl DataTransportRunner for OpenSerialTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let (done_tx, done_rx) = oneshot::channel();
        let channel_id = self.channel_id;
        let reader = SerialReader { port: self.port };
        std::thread::Builder::new()
            .name("serial-rx".to_string())
            .spawn(move || {
                let outcome = run_blocking_receive_loop(channel_id, reader, out, cancel);
                // Report the outcome to the async side (never blocks a worker).
                let _ = done_tx.send(outcome);
            })
            .expect("failed to spawn serial receive thread");
        TransportJoinHandle::Thread(done_rx)
    }
}

/// A blocking, dedicated-thread byte source. `read` blocks up to a bounded
/// timeout and returns `Ok(0)` on timeout (no data yet) so the receive loop can
/// poll cancellation (§111). This seam keeps the loop testable without hardware.
trait BlockingReader: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

/// [`BlockingReader`] backed by a real serial port. Maps the port's `TimedOut`
/// error (raised when the bounded read timeout elapses with no data) to `Ok(0)`.
struct SerialReader {
    port: Box<dyn SerialPort>,
}

impl BlockingReader for SerialReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// The dedicated-thread receive loop (§97.1). Runs until cancelled, the reader
/// reports a fatal error, or the extractor channel closes.
fn run_blocking_receive_loop(
    channel_id: ChannelId,
    mut reader: impl BlockingReader,
    out: Sender<ReceivedData>,
    cancel: CancellationToken,
) -> TransportOutcome {
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        // Cooperative cancellation, observed between bounded reads (§111).
        if cancel.is_cancelled() {
            return TransportOutcome::Cancelled;
        }
        match reader.read(&mut buf) {
            // Timeout / no data: loop back to re-check cancellation.
            Ok(0) => continue,
            Ok(n) => {
                let data = ReceivedData {
                    channel_id,
                    payload: ReceivedPayload::Bytes(buf[..n].to_vec()),
                    received_at: ChunkTime::now(),
                };
                // The one edge that may stall the reader (§97.1, §99): a full
                // extractor queue backpressures here rather than dropping data.
                // An `Err` means the pipeline is gone.
                if out.blocking_send(data).is_err() {
                    return TransportOutcome::Completed;
                }
            }
            // A read error ends the loop as a fault (§94). A UART/driver overrun
            // may have lost bytes before this; that loss is reported here (§101).
            Err(e) => return TransportOutcome::Faulted(format!("serial read failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    /// A [`BlockingReader`] that replays a script of reads, then behaves like an
    /// idle port: a brief sleep + `Ok(0)` (timeout) so the loop polls cancel.
    struct ScriptedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ScriptedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
            }
        }
    }

    impl BlockingReader for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.chunks.pop_front() {
                Some(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ok(n)
                }
                None => {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(0)
                }
            }
        }
    }

    #[tokio::test]
    async fn receive_loop_emits_chunks_then_stops_on_cancellation() {
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"AB".to_vec(), b"CDE".to_vec()]);
        let (done_tx, done_rx) = oneshot::channel();
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(cid, reader, tx, loop_cancel);
            let _ = done_tx.send(outcome);
        });

        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"AB");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"CDE");

        // The reader is now idle; cancellation must end the loop within a timeout.
        cancel.cancel();
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn fatal_read_error_yields_a_faulted_outcome() {
        struct FailingReader;
        impl BlockingReader for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("device gone"))
            }
        }

        let (tx, _rx) = mpsc::channel(4);
        let (done_tx, done_rx) = oneshot::channel();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                ChannelId::new(),
                FailingReader,
                tx,
                CancellationToken::new(),
            );
            let _ = done_tx.send(outcome);
        });
        assert!(matches!(
            done_rx.await.unwrap(),
            TransportOutcome::Faulted(_)
        ));
    }

    #[tokio::test]
    async fn full_queue_stalls_reader_and_preserves_order() {
        // Capacity 1 forces `blocking_send` to stall after each chunk.
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || run_blocking_receive_loop(cid, reader, tx, loop_cancel));

        // The reader stalls rather than dropping: all three arrive, in order
        // (§97.1 — the Transport→Extractor edge stalls instead of losing data).
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"3");

        cancel.cancel();
    }
}
