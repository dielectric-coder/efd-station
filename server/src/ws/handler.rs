use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::pipeline::Pipeline;

/// Shared application state passed to Axum handlers.
pub struct AppState {
    pub pipeline: Pipeline,
    pub cancel: CancellationToken,
}

/// Axum handler: upgrade GET /ws to WebSocket.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_client(socket, state))
}

/// Per-client handler: split socket, spawn downstream + upstream tasks.
async fn handle_client(socket: WebSocket, state: Arc<AppState>) {
    let (sink, stream) = socket.split();

    let fft_rx = state.pipeline.fft_tx.subscribe();
    let state_rx = state.pipeline.state_tx.subscribe();
    let audio_rx = state.pipeline.audio_tx.subscribe();
    let cat_tx = state.pipeline.cat_tx.clone();
    let tx_audio_tx = state.pipeline.tx_audio_tx.clone();
    let cancel = state.cancel.clone();

    info!("WS client connected");

    let cancel2 = cancel.clone();
    let down = tokio::spawn(async move {
        super::downstream::run(sink, fft_rx, state_rx, audio_rx, cancel2).await;
    });

    let up = tokio::spawn(async move {
        super::upstream::run(stream, cat_tx, tx_audio_tx, cancel).await;
    });

    // Wait for either task to finish, then abort the other
    tokio::select! {
        _ = down => { debug!("downstream ended"); }
        _ = up => { debug!("upstream ended"); }
    }

    info!("WS client disconnected");
}
