mod config;
mod pipeline;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::info;

use ws::handler::{ws_upgrade, AppState};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = config::load();
    info!(bind = %cfg.server.bind, port = cfg.server.port, "starting efd-backend");

    let cancel = CancellationToken::new();
    let pipeline = pipeline::Pipeline::start(&cfg);

    let state = Arc::new(AppState {
        pipeline,
        cancel: cancel.clone(),
    });

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let addr = format!("{}:{}", cfg.server.bind, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    info!(addr = %addr, "listening");

    // Serve until SIGINT / SIGTERM
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cancel.clone()))
        .await
        .unwrap();

    // Cancel all pipeline tasks and WS handlers
    info!("shutting down...");
    cancel.cancel();

    // Give tasks time to notice cancellation and exit.
    // spawn_blocking tasks (IQ, ALSA, serial) may be stuck in system calls
    // that don't check cancellation — force exit after grace period.
    tokio::time::sleep(Duration::from_secs(3)).await;

    info!("efd-backend stopped");
    std::process::exit(0);
}

async fn shutdown_signal(cancel: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => info!("SIGINT received"),
        _ = sigterm.recv() => info!("SIGTERM received"),
    }

    #[cfg(not(unix))]
    ctrl_c.await.ok();

    cancel.cancel();
}
