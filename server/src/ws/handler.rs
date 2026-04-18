use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::pipeline::Pipeline;

/// Shared application state passed to Axum handlers.
pub struct AppState {
    pub pipeline: Pipeline,
    pub cancel: CancellationToken,
    /// Optional shared secret required from clients as `?token=<value>`.
    /// `None` disables the check — only safe when bound to loopback.
    pub auth_token: Option<String>,
}

#[derive(Deserialize)]
pub struct AuthParams {
    token: Option<String>,
}

/// Axum handler: upgrade GET /ws to WebSocket.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<AuthParams>,
    State(state): State<Arc<AppState>>,
) -> Response {
    if let Some(expected) = &state.auth_token {
        let got = params.token.as_deref().unwrap_or("");
        if !ct_eq(got.as_bytes(), expected.as_bytes()) {
            warn!("WS upgrade rejected: bad or missing token");
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_client(socket, state)).into_response()
}

// Constant-time byte comparison. Avoids early-exit timing leaks in token check.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Per-client handler: split socket, spawn downstream + upstream tasks.
async fn handle_client(socket: WebSocket, state: Arc<AppState>) {
    let (sink, stream) = socket.split();

    let fft_rx = state.pipeline.fft_tx.subscribe();
    let state_rx = state.pipeline.state_tx.subscribe();
    let audio_rx = state.pipeline.audio_tx.subscribe();
    let cat_tx = state.pipeline.cat_tx.clone();
    let tx_audio_tx = state.pipeline.tx_audio_tx.clone();
    let demod_mode_tx = state.pipeline.demod_mode_tx.clone();
    let audio_source_tx = state.pipeline.audio_source_tx.clone();
    let flip_spectrum_tx = state.pipeline.flip_spectrum_tx.clone();
    let capabilities = state.pipeline.capabilities.clone();
    let drm_status_rx = state.pipeline.drm_status_rx.clone();
    // Phase-2: per-client subscriptions to the shared device list and
    // session snapshot. Downstream pushes on change; upstream mutates
    // them in response to client commands.
    let device_list_rx = state.pipeline.device_list_tx.subscribe();
    let device_list_tx_for_up = state.pipeline.device_list_tx.clone();
    let snapshot_rx = state.pipeline.snapshot_tx.subscribe();
    let snapshot_tx_for_up = state.pipeline.snapshot_tx.clone();
    let cancel = state.cancel.clone();

    info!("WS client connected");

    let cancel2 = cancel.clone();
    let mut down = tokio::spawn(async move {
        super::downstream::run(
            sink,
            capabilities,
            fft_rx,
            state_rx,
            audio_rx,
            drm_status_rx,
            device_list_rx,
            snapshot_rx,
            cancel2,
        )
        .await;
    });

    let mut up = tokio::spawn(async move {
        super::upstream::run(
            stream,
            cat_tx,
            tx_audio_tx,
            demod_mode_tx,
            audio_source_tx,
            flip_spectrum_tx,
            device_list_tx_for_up,
            snapshot_tx_for_up,
            cancel,
        )
        .await;
    });

    // Wait for either task to finish, then abort the other
    tokio::select! {
        result = &mut down => {
            debug!("downstream ended: {:?}", result);
            up.abort();
        }
        result = &mut up => {
            debug!("upstream ended: {:?}", result);
            down.abort();
        }
    }

    info!("WS client disconnected");
}
