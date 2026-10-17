use futures_util::{SinkExt, StreamExt};
use shared_protocol::{ClientSignal, ServerSignal};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum SignalingError {
    #[error("Failed to connect to signaling server at {url}: {source}")]
    Connect {
        url: String,
        source: Box<tokio_tungstenite::tungstenite::Error>,
    },
    #[error("Signaling channel is closed")]
    ChannelClosed,
}

pub struct SignalingClient {
    tx: mpsc::UnboundedSender<ClientSignal>,
    cancel: CancellationToken,
    _io_task: JoinHandle<()>,
}

impl SignalingClient {
    /// Connects to the WebSocket signaling server at `url` and returns the `SignalingClient`
    /// along with the incoming `ServerSignal` stream.
    ///
    /// If the signaling server disconnects later, the background I/O task terminates gracefully
    /// without affecting any active P2P `RTCPeerConnection`s.
    pub async fn connect(
        url: &str,
        cancel: CancellationToken,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ServerSignal>), SignalingError> {
        let (ws_stream, _) =
            connect_async(url)
                .await
                .map_err(|source| SignalingError::Connect {
                    url: url.to_string(),
                    source: Box::new(source),
                })?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ClientSignal>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<ServerSignal>();
        let task_cancel = cancel.clone();

        let io_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => {
                        let _ = ws_sink.close().await;
                        break;
                    }
                    maybe_out = out_rx.recv() => {
                        match maybe_out {
                            Some(signal) => {
                                match serde_json::to_string(&signal) {
                                    Ok(json) => {
                                        if ws_sink.send(WsMessage::Text(json)).await.is_err() {
                                            debug!("Signaling WebSocket sink closed");
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        warn!("Failed to serialize ClientSignal: {err}");
                                    }
                                }
                            }
                            None => break,
                        }
                    }
                    maybe_in = ws_source.next() => {
                        match maybe_in {
                            Some(Ok(WsMessage::Text(text))) => {
                                match serde_json::from_str::<ServerSignal>(&text) {
                                    Ok(server_signal) => {
                                        if in_tx.send(server_signal).is_err() {
                                            break;
                                        }
                                    }
                                    Err(err) => {
                                        warn!("Failed to deserialize ServerSignal: {err}");
                                    }
                                }
                            }
                            Some(Ok(WsMessage::Close(_))) | None => {
                                debug!("Signaling server closed WebSocket connection");
                                break;
                            }
                            Some(Err(err)) => {
                                debug!("Signaling WebSocket disconnected: {err}");
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
        });

        Ok((
            Self {
                tx: out_tx,
                cancel,
                _io_task: io_task,
            },
            in_rx,
        ))
    }

    /// Sends a `ClientSignal` to the signaling server if the WebSocket is still open.
    pub fn send(&self, signal: ClientSignal) -> Result<(), SignalingError> {
        self.tx
            .send(signal)
            .map_err(|_| SignalingError::ChannelClosed)
    }

    pub fn is_connected(&self) -> bool {
        !self.tx.is_closed()
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}
