use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use efd_proto::{AudioChunk, Capabilities, DrmStatus, FftBins, RadioState, ServerMsg};
use futures_util::SinkExt;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Timeout for sending a single WS message. If a client can't receive
/// within this time, disconnect it to avoid blocking broadcasts.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// Minimum interval between DRM status frames forwarded to a client.
/// The DREAM TUI fires roughly every 400 ms; this caps WS spam when
/// multiple clients connect and keeps the UI smooth without being chatty.
const DRM_STATUS_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Downstream task: subscribe to broadcasts, serialize to bincode, send over WS.
pub async fn run(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    capabilities: Capabilities,
    fft_rx: broadcast::Receiver<Arc<FftBins>>,
    state_rx: broadcast::Receiver<RadioState>,
    audio_rx: broadcast::Receiver<AudioChunk>,
    mut drm_status_rx: watch::Receiver<Option<DrmStatus>>,
    cancel: CancellationToken,
) {
    let mut fft_rx = fft_rx;
    let mut state_rx = state_rx;
    let mut audio_rx = audio_rx;
    let mut last_drm_sent = tokio::time::Instant::now() - DRM_STATUS_MIN_INTERVAL;

    let cfg = bincode::config::standard();

    // Send capabilities as the very first message so clients can gate UI
    // before any state arrives.
    match bincode::encode_to_vec(&ServerMsg::Capabilities(capabilities), cfg) {
        Ok(bytes) => {
            let send_fut = sink.send(Message::Binary(Bytes::from(bytes)));
            match tokio::time::timeout(SEND_TIMEOUT, send_fut).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    debug!("WS client disconnected before capabilities sent");
                    let _ = sink.close().await;
                    return;
                }
                Err(_) => {
                    debug!("WS client timed out on capabilities send");
                    let _ = sink.close().await;
                    return;
                }
            }
        }
        Err(e) => warn!("bincode encode error (capabilities): {e}"),
    }

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
            result = drm_status_rx.changed() => {
                if result.is_err() {
                    // supervisor dropped — pipeline shutting down
                    break;
                }
                // Snapshot and rate-limit to avoid WS spam.
                let snap = drm_status_rx.borrow_and_update().clone();
                if last_drm_sent.elapsed() < DRM_STATUS_MIN_INTERVAL {
                    continue;
                }
                last_drm_sent = tokio::time::Instant::now();
                match snap {
                    Some(status) => Some(ServerMsg::DrmStatus(status)),
                    // Intentionally skip None: client holds its last-known
                    // status and hides it itself after a staleness timeout.
                    None => continue,
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
