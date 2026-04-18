mod config;
mod discovery;
mod persistence;
mod pipeline;
mod recording;
mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use ws::handler::{ws_upgrade, AppState};

/// Compile-time server version, surfaced via `--version` and the startup
/// log so operators can match a running binary to a git tag at a glance.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hard cap on how long we wait for pipeline tasks to drain after the
/// HTTP server returns. Some tasks ride on `spawn_blocking` and can be
/// stuck in a syscall; we don't want to deadlock systemd's stop on them.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    // Handle --version / -V before anything else: no tracing init, no
    // config load, no tokio runtime work. Matches the behaviour of most
    // Unix CLIs so `efd-server --version` is cheap and unambiguous.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--version" | "-V" => {
                println!("efd-server {VERSION}");
                return;
            }
            _ => {}
        }
    }

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
    info!(
        version = VERSION,
        bind = %cfg.server.bind,
        port = cfg.server.port,
        "starting efd-backend"
    );

    if !is_loopback_bind(&cfg.server.bind) && cfg.server.auth_token.is_none() {
        warn!(
            bind = %cfg.server.bind,
            "WS is bound to a non-loopback interface with NO auth token. \
             Anyone reachable on this network can control the radio. \
             Set [server] auth_token in config.toml and pass ?token=... from clients."
        );
    }

    // Phase 2 — run device discovery, load persisted state, validate
    // the persisted active_device against the discovered list, and
    // pass both to the pipeline so the session picks up where it
    // left off.
    let mut devices = discovery::enumerate();
    let mut snapshot = persistence::load().unwrap_or_else(persistence::default_snapshot);
    persistence::validate(&mut snapshot, &devices);
    devices.active = snapshot.active_device.clone();
    info!(
        iq_devices = devices.iq_devices.len(),
        audio_devices = devices.audio_devices.len(),
        snapshot_freq = snapshot.freq_hz,
        snapshot_mode = ?snapshot.mode,
        "discovery + persisted state loaded"
    );

    let cancel = CancellationToken::new();
    let pipeline = match std::env::var("EFD_DRM_FILE_TEST").ok() {
        Some(path) if !path.is_empty() => {
            info!(file = %path, "EFD_DRM_FILE_TEST set — starting DRM file-test pipeline");
            pipeline::Pipeline::start_drm_file_test(&cfg, path.into(), cancel.clone())
        }
        _ => pipeline::Pipeline::start(&cfg, devices, snapshot),
    };

    let state = Arc::new(AppState {
        pipeline,
        cancel: cancel.clone(),
        auth_token: cfg.server.auth_token.clone(),
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

    // Phase 3e: also wake the graceful-shutdown future on a
    // client-initiated `SelectDevice`, so the service can cleanly
    // exit and let systemd's `Restart=always` bring it back with
    // the new device active.
    let restart_rx = state.pipeline.restart_requested_tx.subscribe();

    // Serve until SIGINT / SIGTERM / SelectDevice restart request
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(cancel.clone(), restart_rx))
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

    // Grab the current snapshot before tasks are joined (the
    // snapshot-tracker task stops updating once cancel fires, so this
    // captures the last-seen live state).
    let final_snapshot = pipeline.snapshot_tx.borrow().clone();

    match tokio::time::timeout(SHUTDOWN_TIMEOUT, pipeline.shutdown()).await {
        Ok(()) => info!("efd-backend stopped cleanly"),
        Err(_) => warn!(
            "shutdown timeout after {:?}; some hardware tasks may still be in syscalls",
            SHUTDOWN_TIMEOUT
        ),
    }

    // Persist the snapshot last — after all tasks have drained so the
    // freq/mode/BW it carries isn't racing a late CAT poll response.
    persistence::save(&final_snapshot);

    Ok(())
}

fn is_loopback_bind(bind: &str) -> bool {
    // Accept literal forms; also parse IPs to catch exotic loopback addrs
    // (127.0.0.0/8, ::1). Unparseable strings (e.g. "localhost") fall back
    // to the literal check.
    matches!(bind, "localhost" | "127.0.0.1" | "::1")
        || bind
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

async fn shutdown_signal(
    cancel: CancellationToken,
    mut restart_rx: tokio::sync::watch::Receiver<bool>,
) {
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
        r = restart_rx.changed() => match r {
            Ok(()) if *restart_rx.borrow() => {
                info!("client-initiated restart requested — shutting down for systemd respawn");
            }
            _ => {
                // The watch sender was dropped or flipped back to
                // false; either way no action. Fall through to
                // the normal SIGINT/SIGTERM wait.
            }
        },
    }

    #[cfg(not(unix))]
    {
        let _ = restart_rx.changed().await;
        ctrl_c.await.ok();
    }

    cancel.cancel();
}
