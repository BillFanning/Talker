//! Serial transport (spec §14).
//!
//! Unlike UDP/TCP, a serial port has no async-native API, so its continuous
//! blocking receive loop runs on a **dedicated OS thread** that owns the port
//! handle (ADR-001 / §97.1). That thread hands data to the async runtime over a
//! bounded `tokio::sync::mpsc` — the one edge permitted to stall the reader
//! (§97.2, §99): a full Transport→Pipeline queue backpressures the read loop
//! (a bounded retry loop, so the stall notice fires mid-stall and cancellation
//! is observed), which can cause a UART/driver overrun reported as
//! transport-specific loss (§101).
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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, ChunkTime, RuntimeEvent};

use super::{
    DataTransportRunner, ReceivedData, ReceivedPayload, TransportJoinHandle, TransportNotice,
    TransportOutcome,
};

/// Live serial control/status line state (§14.3, §161). Outputs (RTS, DTR) are
/// driven by Listener; inputs (CTS, DSR, DCD, RI) are driven by the device. Output
/// states reflect what Listener has set this session (serial outputs are not
/// read-back), inputs reflect the last poll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SerialControlLines {
    pub rts: bool,
    pub dtr: bool,
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    pub ri: bool,
}

/// A live control-line command to a running serial Channel (§161).
#[derive(Clone, Copy, Debug)]
pub enum SerialControlCommand {
    SetRts(bool),
    SetDtr(bool),
}

/// The runtime's hooks into a running serial reader's control lines (§161): a
/// command inbox, a shared state cell the reader updates, and the event stream for
/// `ControlLinesChanged` signals. The reader polls inputs and applies commands
/// between bounded reads, so this never interferes with reception (§100).
pub struct SerialControlHooks {
    pub commands: Receiver<SerialControlCommand>,
    pub state: Arc<Mutex<SerialControlLines>>,
    pub events: Sender<RuntimeEvent>,
}

/// Read buffer size for one blocking read. A serial read returns whatever bytes
/// are available; the chunk boundary is a reception detail, not stream structure
/// (ADR-010 — the stream is never reframed).
const READ_BUFFER: usize = 4096;

/// Default bounded read timeout. Caps how long a blocking read parks before the
/// loop re-checks cancellation (§111).
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_millis(100);

/// How long the reader must be stalled on the Transport→Pipeline edge before it
/// emits a [`TransportNotice::ReceptionStalled`] (§99, §101; listener ADR-007). A
/// stall this long means the OS/UART receive buffer has had ample time to overrun.
/// Heuristic: momentary backpressure that drains quickly is normal and must not
/// cry loss. The byte count of any overrun is not observable from userland, so the
/// notice carries the stall duration, not a fabricated count (§101).
const STALL_WARNING: Duration = Duration::from_millis(250);

/// Retry cadence while stalled on a full Transport→Pipeline queue. The stall is
/// a poll loop (not a parked `blocking_send`) so the notice can be raised *while*
/// the stall is ongoing — a permanently wedged pipeline must not be silent — and
/// so cancellation still ends the loop (§111). Polling only costs while already
/// stalled, when reception is degraded anyway.
const STALL_POLL: Duration = Duration::from_millis(5);

/// How often the input control lines (CTS/DSR/DCD/RI, §161) are polled. Each poll
/// is four synchronous driver ioctls; doing them before *every* read put four
/// driver round-trips on the hot reception path per chunk — at high chunk rates,
/// far more driver traffic than the data itself. Line changes are human-scale
/// events (a device asserting DTR, a cable unplugged); ~10 Hz shows them as
/// instantly as the GUI can render while costing a bounded ~40 driver calls/s.
/// Pending RTS/DTR **commands** are still applied every pass (operator actions
/// stay immediate), and applying one polls the inputs right away for feedback.
const CONTROL_LINE_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
        match tokio::task::spawn_blocking(move || self.open_blocking()).await {
            Ok(result) => result,
            // A panicked open task (e.g. a driver-provoked panic inside the
            // serial crate) becomes an open error like any other, so the
            // runtime takes Starting → Faulted instead of poisoning the app —
            // production paths never panic.
            Err(join_err) => Err(serialport::Error::new(
                serialport::ErrorKind::Unknown,
                format!("serial open task failed: {join_err}"),
            )),
        }
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
            notices: None,
            control: None,
        })
    }
}

/// An opened serial port ready to receive. Implements [`DataTransportRunner`].
pub struct OpenSerialTransport {
    channel_id: ChannelId,
    port: Box<dyn SerialPort>,
    /// Optional sink for transport notices (§95, §101). When set, a sustained
    /// reader stall sends `ReceptionStalled`; the pipeline turns it into a retained
    /// warning diagnostic and emits the dedicated `RuntimeEvent::ReceptionStalled`
    /// (listener ADR-007).
    notices: Option<Sender<TransportNotice>>,
    /// Optional live control-line hooks (§161): command inbox + state cell + event
    /// sink. When set, the reader services RTS/DTR commands and polls input lines.
    control: Option<SerialControlHooks>,
}

impl OpenSerialTransport {
    pub fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Attach the transport-notice channel so a sustained reader stall — the only
    /// edge that can backpressure the reader (§97.1) — is reported (§101, ADR-007).
    /// Optional: without it the reader still stalls rather than drops; it just
    /// stays silent about it. Serial is the only transport that can stall the
    /// reader, so it is the only one given a notice sink.
    pub fn with_notice_sender(mut self, notices: Sender<TransportNotice>) -> Self {
        self.notices = Some(notices);
        self
    }

    /// Attach live control-line hooks (§161): the reader applies RTS/DTR commands
    /// and polls CTS/DSR/DCD/RI between reads, updating the shared state cell and
    /// signalling `ControlLinesChanged`.
    pub fn with_control(mut self, control: SerialControlHooks) -> Self {
        self.control = Some(control);
        self
    }
}

impl DataTransportRunner for OpenSerialTransport {
    fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
        let (done_tx, done_rx) = oneshot::channel();
        let channel_id = self.channel_id;
        let notices = self.notices;
        let control = self.control;
        let reader = SerialReader { port: self.port };
        let spawned = std::thread::Builder::new()
            .name("serial-rx".to_string())
            .spawn(move || {
                let outcome = run_blocking_receive_loop(
                    channel_id,
                    reader,
                    out,
                    cancel,
                    STALL_WARNING,
                    notices,
                    control,
                );
                // Report the outcome to the async side (never blocks a worker).
                let _ = done_tx.send(outcome);
            });
        if let Err(e) = spawned {
            // Thread spawn can fail on resource exhaustion — plausible on a
            // weeks-long logging host, and never worth a panic (§: no panics
            // in production paths). Report it as a faulted transport: the
            // moved `done_tx` was dropped with the failed closure, so make a
            // fresh pre-completed handle carrying the fault.
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(TransportOutcome::Faulted(format!(
                "failed to spawn the serial receive thread: {e}"
            )));
            return TransportJoinHandle::Thread(rx);
        }
        TransportJoinHandle::Thread(done_rx)
    }
}

/// A blocking, dedicated-thread byte source. `read` blocks up to a bounded
/// timeout and returns `Ok(0)` on timeout (no data yet) so the receive loop can
/// poll cancellation (§111). This seam keeps the loop testable without hardware.
trait BlockingReader: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Read the input/status lines `(CTS, DSR, DCD, RI)` (§14.3). Default: unknown.
    fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
        Ok((false, false, false, false))
    }
    /// Drive the RTS output line (§14.3). Default: no-op.
    fn set_rts(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }
    /// Drive the DTR output line (§14.3). Default: no-op.
    fn set_dtr(&mut self, _on: bool) -> io::Result<()> {
        Ok(())
    }
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

    fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
        Ok((
            self.port.read_clear_to_send().map_err(io::Error::other)?,
            self.port.read_data_set_ready().map_err(io::Error::other)?,
            self.port.read_carrier_detect().map_err(io::Error::other)?,
            self.port.read_ring_indicator().map_err(io::Error::other)?,
        ))
    }

    fn set_rts(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_request_to_send(on)
            .map_err(io::Error::other)
    }

    fn set_dtr(&mut self, on: bool) -> io::Result<()> {
        self.port
            .write_data_terminal_ready(on)
            .map_err(io::Error::other)
    }
}

/// The dedicated-thread receive loop (§97.1). Runs until cancelled, the reader
/// reports a fatal error, or the pipeline channel closes.
///
/// On the Transport→Pipeline edge — the only one permitted to backpressure the
/// reader (§99) — the loop *stalls* rather than drops, so it loses nothing in
/// process. A stall longer than `stall_warning` means the OS/UART buffer has had
/// time to overrun, so it sends a `ReceptionStalled` notice through `notices`
/// (§101, ADR-007) **while the stall is still ongoing** — once per episode,
/// carrying the stall duration so far — so even a permanently wedged pipeline is
/// reported. The lost byte count is not observable from userland, so it is not
/// reported. Cancellation is observed during a stall too (§111).
fn run_blocking_receive_loop(
    channel_id: ChannelId,
    mut reader: impl BlockingReader,
    out: Sender<ReceivedData>,
    cancel: CancellationToken,
    stall_warning: Duration,
    notices: Option<Sender<TransportNotice>>,
    mut control: Option<SerialControlHooks>,
) -> TransportOutcome {
    let mut buf = vec![0u8; READ_BUFFER];
    let mut stall_episodes = 0u64;
    let mut total_stalled = Duration::ZERO;
    let mut max_stall = Duration::ZERO;
    report_serial_stalls(
        &notices,
        channel_id,
        stall_episodes,
        total_stalled,
        max_stall,
    );
    // Live control-line state (§161), tracked across the session.
    let mut lines = SerialControlLines::default();
    // First input-line poll happens immediately (initial state), then throttled.
    let mut next_line_poll = Instant::now();
    loop {
        // Cooperative cancellation, observed between bounded reads (§111).
        if cancel.is_cancelled() {
            return TransportOutcome::Cancelled;
        }
        // Live control lines (§161): apply pending RTS/DTR commands every pass and
        // poll the input lines at a bounded cadence between reads, so this never
        // interferes with reception (§100) nor floods the driver with ioctls.
        if let Some(ctl) = control.as_mut() {
            service_control_lines(
                &mut reader,
                ctl,
                &mut lines,
                channel_id,
                &mut next_line_poll,
            );
        }
        match reader.read(&mut buf) {
            // Timeout / no data: loop back to re-check cancellation.
            Ok(0) => continue,
            Ok(n) => {
                let received_at = ChunkTime::now();
                let data = ReceivedData {
                    channel_id,
                    payload: ReceivedPayload::Bytes(buf[..n].to_vec()),
                    received_at,
                };
                // Fast path: a non-full queue accepts at once and ends any stall
                // episode. On Full we stall — retrying, never dropping (§97.1,
                // §99) — as a poll loop rather than a parked `blocking_send`, so
                // the stall notice can be raised while the stall is *ongoing* (a
                // wedged pipeline must not be silent, ADR-007) and cancellation
                // still ends the loop (§111). A `Closed` queue means the pipeline
                // is gone.
                match out.try_send(data) {
                    Ok(()) => {}
                    Err(TrySendError::Closed(_)) => return TransportOutcome::Completed,
                    Err(TrySendError::Full(data)) => {
                        let stalled_at = Instant::now();
                        let mut pending = data;
                        let mut warned = false;
                        loop {
                            if cancel.is_cancelled() {
                                finish_serial_stall(
                                    &notices,
                                    channel_id,
                                    stalled_at,
                                    &mut stall_episodes,
                                    &mut total_stalled,
                                    &mut max_stall,
                                );
                                return TransportOutcome::Cancelled;
                            }
                            std::thread::sleep(STALL_POLL);
                            match out.try_send(pending) {
                                Ok(()) => {
                                    finish_serial_stall(
                                        &notices,
                                        channel_id,
                                        stalled_at,
                                        &mut stall_episodes,
                                        &mut total_stalled,
                                        &mut max_stall,
                                    );
                                    break;
                                }
                                Err(TrySendError::Closed(_)) => {
                                    finish_serial_stall(
                                        &notices,
                                        channel_id,
                                        stalled_at,
                                        &mut stall_episodes,
                                        &mut total_stalled,
                                        &mut max_stall,
                                    );
                                    return TransportOutcome::Completed;
                                }
                                Err(TrySendError::Full(again)) => {
                                    pending = again;
                                    // A sustained stall risks a UART/driver overrun
                                    // upstream of us — transport-specific loss we flag
                                    // but cannot quantify (§101); once per episode,
                                    // non-blocking (we never block the reader to
                                    // deliver a stall warning). Momentary backpressure
                                    // that drains fast never reaches the threshold.
                                    let waited = stalled_at.elapsed();
                                    if !warned && waited >= stall_warning {
                                        if let Some(notices) = &notices {
                                            let _ = notices.try_send(
                                                TransportNotice::ReceptionStalled {
                                                    channel_id,
                                                    stalled_for: waited,
                                                },
                                            );
                                        }
                                        warned = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // A read error ends the loop as a fault (§94). A UART/driver overrun
            // may have lost bytes before this; that loss is reported here (§101).
            Err(e) => return TransportOutcome::Faulted(format!("serial read failed: {e}")),
        }
    }
}

fn finish_serial_stall(
    notices: &Option<Sender<TransportNotice>>,
    channel_id: ChannelId,
    stalled_at: Instant,
    episodes: &mut u64,
    total: &mut Duration,
    max: &mut Duration,
) {
    let elapsed = stalled_at.elapsed();
    *episodes = episodes.saturating_add(1);
    *total = total.saturating_add(elapsed);
    *max = (*max).max(elapsed);
    report_serial_stalls(notices, channel_id, *episodes, *total, *max);
}

fn report_serial_stalls(
    notices: &Option<Sender<TransportNotice>>,
    channel_id: ChannelId,
    episodes: u64,
    total: Duration,
    max: Duration,
) {
    if let Some(notices) = notices {
        let _ = notices.try_send(TransportNotice::SerialStallSummary {
            channel_id,
            episodes,
            total,
            max,
        });
    }
}

/// Apply any pending RTS/DTR commands and poll the input lines (§161). On any
/// change, update the shared state cell and signal `ControlLinesChanged` (§137) —
/// the cell is the truth, the event is the lightweight signal (ADR-006). A
/// control-line I/O error is ignored (it does not fault the Channel, §96).
///
/// Commands are drained every call; the four input-line ioctls run only when
/// `next_line_poll` is due ([`CONTROL_LINE_POLL_INTERVAL`]) or a command was just
/// applied — they used to run before every read, which at high chunk rates was
/// more driver traffic than the data itself.
fn service_control_lines(
    reader: &mut impl BlockingReader,
    ctl: &mut SerialControlHooks,
    lines: &mut SerialControlLines,
    channel_id: ChannelId,
    next_line_poll: &mut Instant,
) {
    let mut changed = false;
    while let Ok(cmd) = ctl.commands.try_recv() {
        let applied = match cmd {
            SerialControlCommand::SetRts(on) => reader.set_rts(on).map(|()| lines.rts = on),
            SerialControlCommand::SetDtr(on) => reader.set_dtr(on).map(|()| lines.dtr = on),
        };
        changed |= applied.is_ok();
    }
    // A just-applied command re-polls immediately (fresh feedback on lines a
    // driven RTS/DTR may loop back); otherwise honor the cadence.
    let now = Instant::now();
    if !changed && now < *next_line_poll {
        return;
    }
    *next_line_poll = now + CONTROL_LINE_POLL_INTERVAL;
    if let Ok((cts, dsr, dcd, ri)) = reader.read_inputs() {
        if (cts, dsr, dcd, ri) != (lines.cts, lines.dsr, lines.dcd, lines.ri) {
            lines.cts = cts;
            lines.dsr = dsr;
            lines.dcd = dcd;
            lines.ri = ri;
            changed = true;
        }
    }
    if changed {
        if let Ok(mut guard) = ctl.state.lock() {
            *guard = *lines;
        }
        let _ = ctl
            .events
            .try_send(RuntimeEvent::ControlLinesChanged(channel_id));
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

    /// A test reader for control-line behavior: never yields data, exposes mutable
    /// input lines, and records the RTS/DTR it was asked to drive.
    #[derive(Clone, Default)]
    struct ControlReader {
        inputs: Arc<Mutex<(bool, bool, bool, bool)>>, // cts, dsr, dcd, ri
        outputs: Arc<Mutex<(bool, bool)>>,            // rts, dtr
    }

    impl BlockingReader for ControlReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(1));
            Ok(0)
        }
        fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
            Ok(*self.inputs.lock().unwrap())
        }
        fn set_rts(&mut self, on: bool) -> io::Result<()> {
            self.outputs.lock().unwrap().0 = on;
            Ok(())
        }
        fn set_dtr(&mut self, on: bool) -> io::Result<()> {
            self.outputs.lock().unwrap().1 = on;
            Ok(())
        }
    }

    #[tokio::test]
    async fn control_lines_apply_commands_and_report_input_changes() {
        // §161: the reader drives RTS/DTR on command and reports input-line changes
        // via the shared cell + a ControlLinesChanged event.
        let reader = ControlReader::default();
        let inputs = reader.inputs.clone();
        let outputs = reader.outputs.clone();

        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, mut ev_rx) = mpsc::channel(8);
        let state = Arc::new(Mutex::new(SerialControlLines::default()));
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let (tx, _rx) = mpsc::channel(8);

        let hooks = SerialControlHooks {
            commands: cmd_rx,
            state: state.clone(),
            events: ev_tx,
        };
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                None,
                Some(hooks),
            )
        });

        // Drive RTS high: the output line is set, and the cell + event reflect it.
        cmd_tx
            .send(SerialControlCommand::SetRts(true))
            .await
            .unwrap();
        assert_eq!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::ControlLinesChanged(cid)
        );
        assert!(state.lock().unwrap().rts);
        assert!(outputs.lock().unwrap().0);

        // A device asserts CTS: the next poll detects it and reports the change.
        inputs.lock().unwrap().0 = true;
        loop {
            assert_eq!(
                ev_rx.recv().await.unwrap(),
                RuntimeEvent::ControlLinesChanged(cid)
            );
            if state.lock().unwrap().cts {
                break;
            }
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn input_line_polling_is_throttled_not_per_read() {
        // The four input-line ioctls used to run before EVERY read — at high chunk
        // rates, more driver round-trips than the data itself. They are now gated
        // to CONTROL_LINE_POLL_INTERVAL. The idle ControlReader turns a read
        // around in ~1 ms, so ~150 ms of loop means ~150 reads: per-read polling
        // would count ~150; the throttle allows the initial poll plus one due
        // refresh (a generous ceiling absorbs scheduler jitter).
        #[derive(Clone, Default)]
        struct CountingReader {
            inner: ControlReader,
            input_polls: Arc<Mutex<usize>>,
        }
        impl BlockingReader for CountingReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.inner.read(buf)
            }
            fn read_inputs(&mut self) -> io::Result<(bool, bool, bool, bool)> {
                *self.input_polls.lock().unwrap() += 1;
                self.inner.read_inputs()
            }
            fn set_rts(&mut self, on: bool) -> io::Result<()> {
                self.inner.set_rts(on)
            }
            fn set_dtr(&mut self, on: bool) -> io::Result<()> {
                self.inner.set_dtr(on)
            }
        }

        let reader = CountingReader::default();
        let polls = reader.input_polls.clone();
        let (_cmd_tx, cmd_rx) = mpsc::channel(8);
        let (ev_tx, _ev_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(8);
        let hooks = SerialControlHooks {
            commands: cmd_rx,
            state: Arc::new(Mutex::new(SerialControlLines::default())),
            events: ev_tx,
        };
        let loop_cancel = cancel.clone();
        let handle = std::thread::spawn(move || {
            run_blocking_receive_loop(
                ChannelId::new(),
                reader,
                tx,
                loop_cancel,
                STALL_WARNING,
                None,
                Some(hooks),
            )
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
        handle.join().unwrap();

        let count = *polls.lock().unwrap();
        assert!(count >= 1, "the initial input-line poll must happen");
        assert!(
            count <= 5,
            "input polling must follow the ~10 Hz cadence, not per-read \
             (got {count} polls in ~150 ms of ~1 ms reads)"
        );
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
            let outcome =
                run_blocking_receive_loop(cid, reader, tx, loop_cancel, STALL_WARNING, None, None);
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
                STALL_WARNING,
                None,
                None,
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
        std::thread::spawn(move || {
            run_blocking_receive_loop(cid, reader, tx, loop_cancel, STALL_WARNING, None, None)
        });

        // The reader stalls rather than dropping: all three arrive, in order
        // (§97.1 — the Transport→Pipeline edge stalls instead of losing data).
        // This is the §101 in-process boundary: zero loss inside our queues; any
        // loss would be a UART overrun upstream, outside what we can count.
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"3");

        cancel.cancel();
    }

    #[tokio::test]
    async fn sustained_stall_sends_a_reception_stalled_notice() {
        // §101 / ADR-007: a reader stall longer than the threshold sends a
        // ReceptionStalled notice carrying the stall duration — once per episode,
        // and never by dropping data. A short threshold keeps the test quick.
        let (tx, mut rx) = mpsc::channel(1);
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        // Two chunks: the first fills the cap-1 queue; the second stalls the reader
        // until we drain, after a delay that exceeds the threshold.
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let loop_cancel = cancel.clone();
        let threshold = Duration::from_millis(20);
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                Some(notice_tx),
                None,
            )
        });

        assert!(matches!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::SerialStallSummary { episodes: 0, .. }
        ));

        // Let the reader fill the queue and then stall on the second chunk for
        // well over the threshold before we start draining.
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"1");
        assert_eq!(rx.recv().await.unwrap().payload.bytes(), b"2");

        // One warning surfaces while stalled, then a cumulative summary replaces
        // the zero baseline once the queue accepts again.
        match notice_rx.recv().await.unwrap() {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                assert_eq!(channel_id, cid);
                assert!(stalled_for >= threshold);
            }
            other => panic!("expected ReceptionStalled, got {other:?}"),
        }
        match notice_rx.recv().await.unwrap() {
            TransportNotice::SerialStallSummary {
                channel_id,
                episodes,
                total,
                max,
            } => {
                assert_eq!(channel_id, cid);
                assert_eq!(episodes, 1);
                assert!(total >= threshold);
                assert!(max >= threshold);
            }
            other => panic!("expected SerialStallSummary, got {other:?}"),
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn a_wedged_pipeline_raises_the_notice_while_still_stalled_and_can_cancel() {
        // ADR-007 tier 2: the notice must arrive while the stall is ONGOING — a
        // permanently wedged pipeline (never drained) must not be silent. And
        // cancellation must end the stalled reader (§111) even though the queue
        // never drains.
        let (tx, rx) = mpsc::channel(1); // held open, never drained
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        // Chunk 1 fills the cap-1 queue; chunk 2 stalls the reader forever.
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec()]);
        let threshold = Duration::from_millis(20);
        let (done_tx, done_rx) = oneshot::channel();
        let loop_cancel = cancel.clone();
        std::thread::spawn(move || {
            let outcome = run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                Some(notice_tx),
                None,
            );
            let _ = done_tx.send(outcome);
        });

        assert!(matches!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::SerialStallSummary { episodes: 0, .. }
        ));

        // Without draining anything, the notice arrives mid-stall.
        let notice = tokio::time::timeout(Duration::from_secs(5), notice_rx.recv())
            .await
            .expect("the notice must arrive while the stall is ongoing")
            .unwrap();
        match notice {
            TransportNotice::ReceptionStalled {
                channel_id,
                stalled_for,
            } => {
                assert_eq!(channel_id, cid);
                assert!(stalled_for >= threshold);
            }
            other => panic!("expected ReceptionStalled, got {other:?}"),
        }

        // Cancel while still stalled: the loop ends as Cancelled, not hung.
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(5), done_rx)
            .await
            .expect("cancellation must end a stalled reader")
            .unwrap();
        assert!(matches!(outcome, TransportOutcome::Cancelled));
        assert!(matches!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::SerialStallSummary { episodes: 1, .. }
        ));
        drop(rx);
    }

    #[tokio::test]
    async fn momentary_backpressure_updates_totals_without_a_warning() {
        // A queue that drains promptly is normal backpressure, not possible loss:
        // account for the wait but emit no ReceptionStalled warning.
        let (tx, mut rx) = mpsc::channel(1);
        let (notice_tx, mut notice_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let cid = ChannelId::new();
        let reader = ScriptedReader::new(vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]);
        let loop_cancel = cancel.clone();
        // A high threshold the brisk draining below never crosses.
        let threshold = Duration::from_secs(10);
        std::thread::spawn(move || {
            run_blocking_receive_loop(
                cid,
                reader,
                tx,
                loop_cancel,
                threshold,
                Some(notice_tx),
                None,
            )
        });

        assert!(matches!(
            notice_rx.recv().await.unwrap(),
            TransportNotice::SerialStallSummary { episodes: 0, .. }
        ));

        for expected in [b"1", b"2", b"3"] {
            assert_eq!(rx.recv().await.unwrap().payload.bytes(), expected);
        }
        cancel.cancel();

        tokio::time::sleep(Duration::from_millis(20)).await;
        while let Ok(notice) = notice_rx.try_recv() {
            assert!(
                matches!(notice, TransportNotice::SerialStallSummary { .. }),
                "brief backpressure must not raise a warning: {notice:?}"
            );
        }
    }
}
