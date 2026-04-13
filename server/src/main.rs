mod config;
mod pipeline;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use ws::handler::{ws_upgrade, AppState};

/// Hard cap on how long we wait for pipeline tasks to drain after the
/// HTTP server returns. Some tasks ride on `spawn_blocking` and can be
/// stuck in a syscall; we don't want to deadlock systemd's stop on them.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        error!("efd-backend exiting with error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        .with_state(Arc::clone(&state));

    let addr = format!("{}:{}", cfg.server.bind, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    info!(addr = %addr, "listening");

    // Serve until SIGINT / SIGTERM
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cancel.clone()))
        .await
        .map_err(|e| format!("axum serve error: {e}"))?;

    info!("HTTP server stopped, draining pipeline");
    cancel.cancel();

    // Take exclusive ownership of the AppState so we can move the pipeline
    // out for shutdown. Any remaining clones (held by in-flight WS handlers)
    // were cancelled above and should drop momentarily.
    let pipeline = match Arc::try_unwrap(state) {
        Ok(s) => s.pipeline,
        Err(_) => {
            warn!("pipeline still has in-flight references; waiting briefly");
            // Give WS handlers a moment to drop their Arc clones.
            tokio::time::sleep(Duration::from_millis(500)).await;
            // Best-effort: if it's still held, leak the Arc and exit normally.
            // The OS will clean up. Better than panicking on a hot path.
            info!("efd-backend stopped (pipeline still referenced)");
            return Ok(());
        }
    };

    match tokio::time::timeout(SHUTDOWN_TIMEOUT, pipeline.shutdown()).await {
        Ok(()) => info!("efd-backend stopped cleanly"),
        Err(_) => warn!(
            "shutdown timeout after {:?}; some hardware tasks may still be in syscalls",
            SHUTDOWN_TIMEOUT
        ),
    }

    Ok(())
}

async fn shutdown_signal(cancel: CancellationToken) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(s) => s,
        Err(e) => {
            warn!("cannot install SIGTERM handler: {e} — relying on Ctrl-C only");
            let _ = ctrl_c.await;
            cancel.cancel();
            return;
        }
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => info!("SIGINT received"),
        _ = sigterm.recv() => info!("SIGTERM received"),
    }

    #[cfg(not(unix))]
    ctrl_c.await.ok();

    cancel.cancel();
}
