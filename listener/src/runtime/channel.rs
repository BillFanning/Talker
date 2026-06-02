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

use tokio::sync::mpsc::{self, Sender};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, RuntimeEvent};
use crate::decode::Decoder;
use crate::extract::MessageExtractor;
use crate::transport::{DataTransportRunner, TransportJoinHandle};

use super::pipeline::{run_channel, ChannelPipeline, PipelineCapacities};

/// The transport + pipeline tasks for one Channel and the controls to stop
/// them. Shared internal building block behind [`RunningChannel`] and the TCP
/// listener supervisor.
pub(crate) struct ChannelTasks {
    pub(crate) channel_id: ChannelId,
    pub(crate) transport_cancel: CancellationToken,
    pub(crate) pipeline_cancel: CancellationToken,
    pub(crate) transport: TransportJoinHandle,
    pub(crate) pipeline_task: JoinHandle<ChannelPipeline>,
}

impl ChannelTasks {
    /// Graceful stop (§110): stop reception, close the interface, then let the
    /// pipeline drain the accepted backlog (its sender drops when the transport
    /// task ends, so its receive loop sees the channel close and returns).
    pub(crate) async fn stop(self) -> ChannelPipeline {
        self.transport_cancel.cancel();
        self.transport.join().await;
        self.pipeline_task
            .await
            .expect("pipeline task should not panic")
    }

    /// Forced stop (§111, §113): cancel transport and pipeline together; a
    /// queued backlog may be abandoned rather than drained.
    pub(crate) async fn abort(self) -> ChannelPipeline {
        self.transport_cancel.cancel();
        self.pipeline_cancel.cancel();
        self.transport.join().await;
        self.pipeline_task
            .await
            .expect("pipeline task should not panic")
    }
}

/// Wire a bound data-bearing transport to a fresh pipeline and start both tasks,
/// emitting events into the supplied sender (§97.2, §102, §137).
pub(crate) fn spawn_channel_tasks<R: DataTransportRunner>(
    channel_id: ChannelId,
    runner: R,
    extractor: Box<dyn MessageExtractor + Send>,
    decoder: Option<Box<dyn Decoder + Send>>,
    caps: PipelineCapacities,
    raw_recording: bool,
    events: Sender<RuntimeEvent>,
) -> ChannelTasks {
    // The bounded Transport→Extractor queue — the only edge that may stall the
    // reader (§97.1, §99). No unbounded intermediate queue is introduced (§97.2).
    let (ingest_tx, ingest_rx) = mpsc::channel(caps.ingest);

    let mut pipeline =
        ChannelPipeline::new(channel_id, extractor, caps, raw_recording).with_event_sender(events);
    if let Some(decoder) = decoder {
        pipeline = pipeline.with_decoder(decoder);
    }

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
    }
}

/// A live, standalone Channel: its tasks plus its own event receiver. Returned
/// by [`start_data_channel`].
pub struct RunningChannel {
    tasks: ChannelTasks,
    events: mpsc::Receiver<RuntimeEvent>,
}

impl RunningChannel {
    pub fn channel_id(&self) -> ChannelId {
        self.tasks.channel_id
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
    raw_recording: bool,
) -> RunningChannel {
    let (event_tx, event_rx) = mpsc::channel(caps.events);
    let tasks = spawn_channel_tasks(
        channel_id,
        runner,
        extractor,
        None,
        caps,
        raw_recording,
        event_tx,
    );
    RunningChannel {
        tasks,
        events: event_rx,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ChunkTime, RuntimeEvent};
    use crate::extract::StreamExtractor;
    use crate::transport::udp::{UdpMode, UdpTransport};
    use crate::transport::{ReceivedData, ReceivedPayload};
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
                        return;
                    }
                }
                cancel.cancelled().await;
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
            false,
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
            false,
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
            false,
        );

        let pipeline = tokio::time::timeout(Duration::from_secs(5), running.abort())
            .await
            .expect("forced abort hung");
        // It terminated; it cannot have retained more than was produced.
        assert!(pipeline.retention().len() <= 1000);
    }
}
