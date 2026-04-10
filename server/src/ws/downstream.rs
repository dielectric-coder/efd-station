use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use efd_proto::{AudioChunk, FftBins, RadioState, ServerMsg};
use futures_util::SinkExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Timeout for sending a single WS message. If a client can't receive
/// within this time, disconnect it to avoid blocking broadcasts.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Downstream task: subscribe to broadcasts, serialize to bincode, send over WS.
pub async fn run(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    fft_rx: broadcast::Receiver<Arc<FftBins>>,
    state_rx: broadcast::Receiver<RadioState>,
    audio_rx: broadcast::Receiver<AudioChunk>,
    cancel: CancellationToken,
) {
    let mut fft_rx = fft_rx;
    let mut state_rx = state_rx;
    let mut audio_rx = audio_rx;

    let cfg = bincode::config::standard();

    loop {
        // Select whichever broadcast fires first
        let msg: Option<ServerMsg> = tokio::select! {
            _ = cancel.cancelled() => break,
            result = fft_rx.recv() => {
                match result {
                    Ok(bins) => Some(ServerMsg::FftBins((*bins).clone())),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        trace!(skipped = n, "WS downstream: FFT lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = state_rx.recv() => {
                match result {
                    Ok(state) => Some(ServerMsg::RadioState(state)),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        trace!(skipped = n, "WS downstream: state lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            result = audio_rx.recv() => {
                match result {
                    Ok(chunk) => Some(ServerMsg::Audio(chunk)),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        trace!(skipped = n, "WS downstream: audio lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        };

        let Some(msg) = msg else { break };

        match bincode::encode_to_vec(&msg, cfg) {
            Ok(bytes) => {
                let send_fut = sink.send(Message::Binary(Bytes::from(bytes)));
                match tokio::time::timeout(SEND_TIMEOUT, send_fut).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        debug!("WS client disconnected (send failed)");
                        break;
                    }
                    Err(_) => {
                        debug!("WS client too slow (send timeout), disconnecting");
                        break;
                    }
                }
            }
            Err(e) => {
                warn!("bincode encode error: {e}");
            }
        }
    }

    let _ = sink.close().await;
}
