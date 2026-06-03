//! Channel orchestration: wiring a transport to its pipeline and driving the
//! Channel lifecycle (spec §10, §97, §110, §111).
//!
//! [`start_data_channel`] is the runtime's per-Channel setup for a single
//! data-bearing transport (Serial, UDP, or a standalone TCP connection): it
//! creates the bounded Transport→Extractor queue (§97.2), spawns the pipeline
//! task, and hands the transport its `out` sender. The returned
//! [`RunningChannel`] owns the cancellation tokens and join handles for
//! shutdown, plus its own event receiver.
//!
//! [`spawn_channel_tasks`] is the lower-level primitive: it takes an externally
//! supplied event sender (so many channels can share one event stream, as the
//! TCP listener supervisor does) and returns the raw [`ChannelTasks`].
//!
//! Two shutdown paths:
//! - graceful (§110): stop reception first, then let the pipeline drain the
//!   already-queued accepted data before finishing.
//! - forced (§111, §113): cancel both halves at once without guaranteeing the
//!   backlog is drained.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc::{self, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, RuntimeEvent};
use crate::decode::Decoder;
use crate::display::{DisplayView, RenderedOutput};
use crate::extract::MessageExtractor;
use crate::record::Recording;
use crate::transport::{DataTransportRunner, ReceivedData, TransportJoinHandle, TransportOutcome};

use super::pipeline::{run_channel, ChannelPipeline, DisplayViewHandle, PipelineCapacities};

/// The transport + pipeline tasks for one Channel. A plain holder, destructured
/// by [`spawn_monitored_channel`] and the TCP listener supervisor (which each do
/// their own transport-outcome monitoring).
pub(crate) struct ChannelTasks {
    pub(crate) channel_id: ChannelId,
    pub(crate) transport_cancel: CancellationToken,
    pub(crate) pipeline_cancel: CancellationToken,
    pub(crate) transport: TransportJoinHandle,
    pub(crate) pipeline_task: JoinHandle<ChannelPipeline>,
    pub(crate) display_handles: Vec<DisplayViewHandle>,
}

/// Wire a bound data-bearing transport to a fresh pipeline and start both tasks,
/// emitting events into the supplied sender (§97.2, §102, §137).
// Internal wiring with distinct, meaningful per-channel inputs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_channel_tasks<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    extractor: Box<dyn MessageExtractor + Send>,
    decoder: Option<Box<dyn Decoder + Send>>,
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
    display_view_count: usize,
    caps: PipelineCapacities,
    events: Sender<RuntimeEvent>,
) -> ChannelTasks {
    // The bounded Transport→Extractor queue — the only edge that may stall the
    // reader (§97.1, §99). No unbounded intermediate queue is introduced (§97.2).
    let (ingest_tx, ingest_rx) = mpsc::channel(caps.ingest);

    let mut pipeline = ChannelPipeline::new(channel_id, extractor, caps).with_event_sender(events);
    if let Some(decoder) = decoder {
        pipeline = pipeline.with_decoder(decoder);
    }
    if let Some(recorder) = raw_recorder {
        pipeline = pipeline.with_raw_recorder(recorder);
    }
    // `new` created the default Display View; add the rest to reach the count.
    for _ in 1..display_view_count.max(1) {
        pipeline.add_display_view(caps.display);
    }
    if let Some((renderer, recording)) = display_recorder {
        pipeline.set_display_recorder(renderer, recording);
    }
    let display_handles = pipeline.display_view_handles();

    let transport_cancel = CancellationToken::new();
    let pipeline_cancel = CancellationToken::new();

    let transport = runner.run(ingest_tx, transport_cancel.clone());
    let pipeline_task = tokio::spawn(run_channel(ingest_rx, pipeline, pipeline_cancel.clone()));

    ChannelTasks {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        transport,
        pipeline_task,
        display_handles,
    }
}

/// A data Channel's controls plus a fault monitor. The transport's join handle
/// lives in the monitor task — which emits `ChannelFaulted` if the transport
/// ends on a spontaneous fault (§94/§101) — so `stop`/`abort` synchronize on the
/// pipeline draining rather than joining the transport directly.
pub(crate) struct MonitoredChannel {
    channel_id: ChannelId,
    transport_cancel: CancellationToken,
    pipeline_cancel: CancellationToken,
    pipeline_task: JoinHandle<ChannelPipeline>,
    monitor: JoinHandle<()>,
    display_handles: Vec<DisplayViewHandle>,
}

impl MonitoredChannel {
    /// Pause/resume handles for this Channel's Display Views (§11, §48).
    pub(crate) fn display_handles(&self) -> &[DisplayViewHandle] {
        &self.display_handles
    }

    /// Graceful stop (§110): stop reception; the transport's sender drops, the
    /// pipeline drains the accepted backlog and returns, and the monitor ends.
    pub(crate) async fn stop(self) -> ChannelPipeline {
        self.transport_cancel.cancel();
        let pipeline = self
            .pipeline_task
            .await
            .expect("pipeline task should not panic");
        let _ = self.monitor.await;
        pipeline
    }

    /// Forced stop (§111, §113): cancel both halves; the backlog may be abandoned.
    pub(crate) async fn abort(self) -> ChannelPipeline {
        self.transport_cancel.cancel();
        self.pipeline_cancel.cancel();
        let pipeline = self
            .pipeline_task
            .await
            .expect("pipeline task should not panic");
        let _ = self.monitor.await;
        pipeline
    }
}

/// Like [`spawn_channel_tasks`] but adds a fault monitor that awaits the
/// transport outcome and emits `ChannelFaulted` if it ended on a fault (§94,
/// §101). Used for standalone and orchestrated data channels; the TCP supervisor
/// does its own per-connection monitoring instead.
///
/// `faulted` is the shared per-channel fault flag (listener ADR-006): the
/// detached monitor cannot mutate the method-based orchestrator's state, so it
/// flips this flag on a spontaneous fault. The orchestrator reads it to keep
/// `state()` and command validation honest; observers learn of the fault through
/// the `ChannelFaulted` event, which is authoritative for presentation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_monitored_channel<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    extractor: Box<dyn MessageExtractor + Send>,
    decoder: Option<Box<dyn Decoder + Send>>,
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
    display_recorder: Option<(DisplayView, Recording<RenderedOutput>)>,
    display_view_count: usize,
    caps: PipelineCapacities,
    events: Sender<RuntimeEvent>,
    faulted: Arc<AtomicBool>,
) -> MonitoredChannel {
    let monitor_events = events.clone();
    let ChannelTasks {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        transport,
        pipeline_task,
        display_handles,
    } = spawn_channel_tasks(
        channel_id,
        runner,
        extractor,
        decoder,
        raw_recorder,
        display_recorder,
        display_view_count,
        caps,
        events,
    );

    let monitor = tokio::spawn(async move {
        // A spontaneous fault surfaces as ChannelFaulted; cancel/EOF/pipeline-gone
        // outcomes are normal ends and emit nothing. The flag reconciles the
        // orchestrator's internal state (ADR-006); the event informs observers.
        if let TransportOutcome::Faulted(_) = transport.join().await {
            faulted.store(true, Ordering::Relaxed);
            let _ = monitor_events.try_send(RuntimeEvent::ChannelFaulted(channel_id));
        }
    });

    MonitoredChannel {
        channel_id,
        transport_cancel,
        pipeline_cancel,
        pipeline_task,
        monitor,
        display_handles,
    }
}

/// A live, standalone Channel: its tasks plus its own event receiver. Returned
/// by [`start_data_channel`].
pub struct RunningChannel {
    tasks: MonitoredChannel,
    events: mpsc::Receiver<RuntimeEvent>,
    faulted: Arc<AtomicBool>,
}

impl RunningChannel {
    pub fn channel_id(&self) -> ChannelId {
        self.tasks.channel_id
    }

    /// Whether the transport has ended on a spontaneous fault (§94/§101). Set by
    /// the fault monitor; the `ChannelFaulted` event fires at the same time.
    pub fn is_faulted(&self) -> bool {
        self.faulted.load(Ordering::Relaxed)
    }

    /// The runtime→UI event stream for this Channel (§137).
    pub fn events(&mut self) -> &mut mpsc::Receiver<RuntimeEvent> {
        &mut self.events
    }

    /// Graceful stop (§110).
    pub async fn stop(self) -> ChannelPipeline {
        self.tasks.stop().await
    }

    /// Forced stop (§111, §113).
    pub async fn abort(self) -> ChannelPipeline {
        self.tasks.abort().await
    }
}

/// Start a standalone data channel (Serial, UDP, or a lone TCP connection). For
/// a datagram transport (UDP) the `extractor` is unused because datagrams bypass
/// extraction (§15) — pass a `StreamExtractor`.
pub fn start_data_channel<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    extractor: Box<dyn MessageExtractor + Send>,
    caps: PipelineCapacities,
    raw_recorder: Option<Recording<Arc<ReceivedData>>>,
) -> RunningChannel {
    let (event_tx, event_rx) = mpsc::channel(caps.events);
    let faulted = Arc::new(AtomicBool::new(false));
    let tasks = spawn_monitored_channel(
        channel_id,
        runner,
        extractor,
        None,
        raw_recorder,
        None, // no display recording on a standalone channel
        1,    // a single default Display View
        caps,
        event_tx,
        faulted.clone(),
    );
    RunningChannel {
        tasks,
        events: event_rx,
        faulted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChunkTime, RuntimeEvent};
    use crate::extract::StreamExtractor;
    use crate::transport::udp::{UdpMode, UdpTransport};
    use crate::transport::{ReceivedData, ReceivedPayload, TransportOutcome};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// A test transport that emits a fixed script of datagrams, then stays alive
    /// until cancelled. Lets shutdown tests control the accepted backlog.
    struct ScriptedTransport {
        channel_id: ChannelId,
        chunks: Vec<Vec<u8>>,
    }

    impl DataTransportRunner for ScriptedTransport {
        fn run(self, out: Sender<ReceivedData>, cancel: CancellationToken) -> TransportJoinHandle {
            let ScriptedTransport { channel_id, chunks } = self;
            let handle = tokio::spawn(async move {
                for chunk in chunks {
                    let data = ReceivedData {
                        channel_id,
                        payload: ReceivedPayload::Datagram(chunk),
                        received_at: ChunkTime::now(),
                    };
                    // Stops early if the pipeline is gone (forced shutdown).
                    if out.send(data).await.is_err() {
                        return TransportOutcome::Completed;
                    }
                }
                cancel.cancelled().await;
                TransportOutcome::Cancelled
            });
            TransportJoinHandle::Task(handle)
        }
    }

    #[tokio::test]
    async fn udp_channel_end_to_end_through_the_pipeline() {
        // Bind a UDP transport and run it through the orchestrated pipeline.
        let transport = UdpTransport::new(
            ChannelId::new(),
            "127.0.0.1:0".parse().unwrap(),
            UdpMode::Unicast,
        );
        let bound = transport.bind().await.unwrap();
        let server_addr = bound.local_addr().unwrap();
        let channel_id = bound.channel_id();

        let mut running = start_data_channel(
            channel_id,
            bound,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
            None,
        );

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"alpha", server_addr).await.unwrap();
        client.send_to(b"bravo", server_addr).await.unwrap();

        // Await two MessageReceived events — deterministic synchronization.
        for expected in 1..=2u64 {
            match running.events().recv().await.unwrap() {
                RuntimeEvent::MessageReceived(cid, number) => {
                    assert_eq!(cid, channel_id);
                    assert_eq!(number, expected);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }

        // Graceful stop drains and returns the pipeline; both datagrams retained,
        // each one Message, numbered in receive order.
        let pipeline = running.stop().await;
        let retained: Vec<(u64, Vec<u8>)> = pipeline
            .retention()
            .iter()
            .map(|d| (d.message.number, d.message.bytes.to_vec()))
            .collect();
        assert_eq!(
            retained,
            vec![(1, b"alpha".to_vec()), (2, b"bravo".to_vec())]
        );
    }

    #[tokio::test]
    async fn graceful_stop_drains_the_accepted_backlog() {
        // §110: graceful stop must finish processing already-accepted data.
        let cid = ChannelId::new();
        let transport = ScriptedTransport {
            channel_id: cid,
            chunks: (0u8..5).map(|i| vec![i]).collect(),
        };
        let running = start_data_channel(
            cid,
            transport,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
            None,
        );

        let pipeline = tokio::time::timeout(Duration::from_secs(5), running.stop())
            .await
            .expect("graceful stop hung");
        // All five datagrams were drained before finalizing.
        assert_eq!(pipeline.retention().len(), 5);
    }

    #[tokio::test]
    async fn forced_abort_completes_without_hanging() {
        // §111: forced shutdown may abandon the backlog, but must terminate
        // promptly and cleanly rather than draining it.
        let cid = ChannelId::new();
        let transport = ScriptedTransport {
            channel_id: cid,
            chunks: (0..1000).map(|i| vec![(i % 256) as u8]).collect(),
        };
        let running = start_data_channel(
            cid,
            transport,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
            None,
        );

        let pipeline = tokio::time::timeout(Duration::from_secs(5), running.abort())
            .await
            .expect("forced abort hung");
        // It terminated; it cannot have retained more than was produced.
        assert!(pipeline.retention().len() <= 1000);
    }

    #[tokio::test]
    async fn spontaneous_transport_fault_emits_channel_faulted() {
        // §94/§101: a transport that ends on a fault (not a stop) surfaces a
        // ChannelFaulted event via the channel's fault monitor.
        struct FaultingTransport;
        impl DataTransportRunner for FaultingTransport {
            fn run(
                self,
                _out: Sender<ReceivedData>,
                _cancel: CancellationToken,
            ) -> TransportJoinHandle {
                TransportJoinHandle::Task(tokio::spawn(async move {
                    TransportOutcome::Faulted("device error".to_string())
                }))
            }
        }

        let cid = ChannelId::new();
        let mut running = start_data_channel(
            cid,
            FaultingTransport,
            Box::new(StreamExtractor::new()),
            PipelineCapacities::default(),
            None,
        );
        assert_eq!(
            running.events().recv().await.unwrap(),
            RuntimeEvent::ChannelFaulted(cid)
        );
        // The same fault flips the shared state flag (ADR-006), so an orchestrator
        // reading it reconciles to Faulted without waiting for a command.
        assert!(running.is_faulted());
        let _ = running.stop().await;
    }
}
