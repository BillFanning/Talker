//! TCP listener supervision (spec §16, §97.1).
//!
//! A TCP listener does not itself receive application data — it accepts clients
//! and the runtime turns each into an independent TCP connection channel (Model
//! A, §16.4). This supervisor task:
//!
//! - runs the listener acceptor and, per [`NewConnection`](crate::transport::NewConnection),
//!   mints a fresh `ChannelId` (§97.1) and starts a connection channel;
//! - enforces `max_connections` (§16.1): at the limit it closes the incoming
//!   connection and raises a `ConnectionRejected` warning (§93) without faulting
//!   the listener;
//! - emits `TcpClientConnected` / `TcpClientDisconnected` lifecycle events
//!   (§137), routing every connection's `MessageReceived` into the same shared
//!   event stream;
//! - on stop, stops accepting and then stops every live connection (§110, §13).
//!
//! Connection channels are runtime-only and never persisted (§16.3).

use std::collections::HashMap;

use tokio::sync::mpsc::{self, Sender};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::core::{ChannelId, RuntimeEvent};
use crate::decode::Decoder;
use crate::extract::MessageExtractor;
use crate::transport::tcp::{BoundTcpListenerTransport, TcpConnectionTransport};
use crate::transport::{ConnectionAcceptorRunner, NewConnection, TransportOutcome};

use super::channel::{spawn_channel_tasks, ChannelTasks, TRANSPORT_NOTICES};
use super::pipeline::PipelineCapacities;

/// Per-connection state retained by the supervisor for shutdown and disconnect
/// handling. The transport join handle lives in the monitor `JoinSet`, not here.
struct Connection {
    transport_cancel: CancellationToken,
    pipeline_task: JoinHandle<super::pipeline::ChannelPipeline>,
}

/// Handle to a running TCP listener and its connection channels.
pub struct TcpListenerHandle {
    listener_id: ChannelId,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl TcpListenerHandle {
    pub fn listener_id(&self) -> ChannelId {
        self.listener_id
    }

    /// Graceful shutdown (§110, §13): stop accepting, then stop all connections.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Start a TCP listener and supervise its connection channels (§16).
///
/// `make_extractor` and `make_decoder` build a fresh extractor/decoder per
/// connection, so each accepted connection inherits the listener's configured
/// extraction and decoding with independent state (§16.2). All events —
/// lifecycle and per-connection `MessageReceived` — flow into `events`.
///
/// Per-connection **recording** is not wired: each connection would need a
/// distinct destination file, which depends on filename templating (§59,
/// deferred). Accepted connections are therefore unrecorded for now.
pub fn start_tcp_listener<F, D>(
    bound: BoundTcpListenerTransport,
    make_extractor: F,
    make_decoder: D,
    caps: PipelineCapacities,
    max_connections: Option<u32>,
    events: Sender<RuntimeEvent>,
) -> TcpListenerHandle
where
    F: Fn() -> Box<dyn MessageExtractor + Send> + Send + 'static,
    D: Fn() -> Option<Box<dyn Decoder + Send>> + Send + 'static,
{
    let listener_id = bound.channel_id();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();

    let task = tokio::spawn(async move {
        // Listener acceptor → supervisor queue of NewConnection.
        let (conn_tx, mut conn_rx) = mpsc::channel::<NewConnection>(caps.ingest);
        let listener_cancel = CancellationToken::new();
        let listener_handle = bound.run(conn_tx, listener_cancel.clone());

        let mut connections: HashMap<ChannelId, Connection> = HashMap::new();
        // Each entry awaits a connection's transport completion and yields its id.
        let mut monitors: JoinSet<(ChannelId, TransportOutcome)> = JoinSet::new();
        let mut listener_open = true;

        loop {
            tokio::select! {
                biased;
                _ = task_cancel.cancelled() => break,

                maybe = conn_rx.recv(), if listener_open => match maybe {
                    Some(new_conn) => {
                        // max_connections (§16.1): reject at the limit without faulting.
                        if let Some(max) = max_connections {
                            if connections.len() as u32 >= max {
                                drop(new_conn.stream); // close the rejected connection
                                let _ = events.try_send(RuntimeEvent::WarningRaised(listener_id));
                                continue;
                            }
                        }

                        let conn_id = ChannelId::new(); // runtime mints (§97.1)
                        let transport = TcpConnectionTransport::new(conn_id, new_conn.stream);
                        // TCP is async and never stalls the reader (§97.1), so it
                        // sends no transport notices; drop the sender.
                        let (_notice_tx, notice_rx) = mpsc::channel(TRANSPORT_NOTICES);
                        let tasks = spawn_channel_tasks(
                            conn_id,
                            transport,
                            make_extractor(),
                            make_decoder(),
                            // Per-connection recording is deferred (distinct files
                            // per connection need §59 filename templates).
                            None,
                            None,
                            1, // one default Display View per connection
                            caps,
                            events.clone(),
                            notice_rx,
                        );
                        let ChannelTasks {
                            transport: transport_join,
                            transport_cancel,
                            pipeline_task,
                            channel_id: _,
                            pipeline_cancel: _,
                            display_handles: _,
                            // Per-connection snapshots are deferred (no per-conn
                            // handle is retained); drop the request sender.
                            snapshots: _,
                        } = tasks;

                        // Detect disconnect: the transport task ends on EOF/cancel/fault.
                        monitors.spawn(async move {
                            let outcome = transport_join.join().await;
                            (conn_id, outcome)
                        });
                        connections.insert(conn_id, Connection { transport_cancel, pipeline_task });
                        let _ = events.try_send(RuntimeEvent::TcpClientConnected(conn_id));
                    }
                    None => listener_open = false, // acceptor ended; existing connections live on
                },

                Some(joined) = monitors.join_next(), if !monitors.is_empty() => {
                    if let Ok((conn_id, outcome)) = joined {
                        if let Some(conn) = connections.remove(&conn_id) {
                            // Reception ended; drain the accepted backlog (§110).
                            let _ = conn.pipeline_task.await;
                            // A read fault is reported before the disconnect (§94/§101).
                            if let TransportOutcome::Faulted(_) = outcome {
                                let _ = events.try_send(RuntimeEvent::ChannelFaulted(conn_id));
                            }
                            let _ = events.try_send(RuntimeEvent::TcpClientDisconnected(conn_id));
                        }
                    }
                }
            }
        }

        // Shutdown (§110, §13): stop accepting, then stop every connection.
        listener_cancel.cancel();
        let _ = listener_handle.join().await;
        for (conn_id, conn) in connections.drain() {
            conn.transport_cancel.cancel();
            let _ = conn.pipeline_task.await;
            let _ = events.try_send(RuntimeEvent::TcpClientDisconnected(conn_id));
        }
        monitors.shutdown().await;
    });

    TcpListenerHandle {
        listener_id,
        cancel,
        task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{DelimiterExtractor, StreamExtractor};
    use crate::transport::tcp::TcpListenerTransport;
    use std::collections::{HashMap, HashSet};
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    async fn bound_listener() -> BoundTcpListenerTransport {
        TcpListenerTransport::new(ChannelId::new(), "127.0.0.1:0".parse().unwrap())
            .bind()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn connection_lifecycle_emits_connected_message_and_disconnected() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let listener_id = bound.channel_id();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);

        let handle = start_tcp_listener(
            bound,
            || Box::new(DelimiterExtractor::new(vec![b'\n'], false)),
            || None::<Box<dyn Decoder + Send>>,
            PipelineCapacities::default(),
            None,
            ev_tx,
        );

        let mut client = TcpStream::connect(addr).await.unwrap();

        let conn_id = match ev_rx.recv().await.unwrap() {
            RuntimeEvent::TcpClientConnected(id) => {
                assert_ne!(id, listener_id);
                id
            }
            other => panic!("expected TcpClientConnected, got {other:?}"),
        };

        client.write_all(b"hello\n").await.unwrap();
        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::MessageReceived(id, number) => {
                assert_eq!(id, conn_id);
                assert_eq!(number, 1);
            }
            other => panic!("expected MessageReceived, got {other:?}"),
        }

        drop(client); // client disconnects → EOF
        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::TcpClientDisconnected(id) => assert_eq!(id, conn_id),
            other => panic!("expected TcpClientDisconnected, got {other:?}"),
        }

        handle.stop().await;
    }

    #[tokio::test]
    async fn max_connections_rejects_and_warns_without_faulting() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let listener_id = bound.channel_id();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);

        let handle = start_tcp_listener(
            bound,
            || Box::new(StreamExtractor::new()),
            || None::<Box<dyn Decoder + Send>>,
            PipelineCapacities::default(),
            Some(1),
            ev_tx,
        );

        // First client accepted.
        let _c1 = TcpStream::connect(addr).await.unwrap();
        assert!(matches!(
            ev_rx.recv().await.unwrap(),
            RuntimeEvent::TcpClientConnected(_)
        ));

        // Second client over the limit: rejected with a warning on the listener.
        let _c2 = TcpStream::connect(addr).await.unwrap();
        match ev_rx.recv().await.unwrap() {
            RuntimeEvent::WarningRaised(id) => assert_eq!(id, listener_id),
            other => panic!("expected WarningRaised, got {other:?}"),
        }

        handle.stop().await;
    }

    #[tokio::test]
    async fn connections_are_independent_with_per_connection_numbering() {
        let bound = bound_listener().await;
        let addr = bound.local_addr().unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(128);

        let handle = start_tcp_listener(
            bound,
            || Box::new(DelimiterExtractor::new(vec![b'\n'], false)),
            || None::<Box<dyn Decoder + Send>>,
            PipelineCapacities::default(),
            None,
            ev_tx,
        );

        let mut c1 = TcpStream::connect(addr).await.unwrap();
        let mut c2 = TcpStream::connect(addr).await.unwrap();
        c1.write_all(b"a\n").await.unwrap();
        c2.write_all(b"b\n").await.unwrap();

        // Events from two connections interleave; collect by counts.
        let mut connected: HashSet<ChannelId> = HashSet::new();
        let mut numbers: HashMap<ChannelId, u64> = HashMap::new();
        while connected.len() < 2 || numbers.len() < 2 {
            match ev_rx.recv().await.unwrap() {
                RuntimeEvent::TcpClientConnected(id) => {
                    connected.insert(id);
                }
                RuntimeEvent::MessageReceived(id, number) => {
                    numbers.insert(id, number);
                }
                _ => {}
            }
        }

        assert_eq!(connected.len(), 2, "two distinct connection channels");
        assert_eq!(numbers.len(), 2);
        // Each connection numbers independently from 1 (Model A, §16.2).
        assert!(numbers.values().all(|&n| n == 1));

        handle.stop().await;
    }
}
