use std::time::Duration;

use efd_proto::{AgcMode, CatCommand, RadioState};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::client::RigctldConn;
use crate::error::CatError;
use crate::parse;

/// Configuration for the CAT tasks.
#[derive(Debug, Clone)]
pub struct CatConfig {
    pub host: String,
    pub port: u16,
    /// Polling interval for radio state (default: 200ms).
    pub poll_interval: Duration,
}

impl Default for CatConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 4532,
            poll_interval: Duration::from_millis(200),
        }
    }
}

/// Spawn the CAT poll + command tasks.
///
/// Returns handles for both tasks. They share a single TCP connection
/// serialized through an internal channel.
pub fn spawn_cat_tasks(
    config: CatConfig,
    state_tx: broadcast::Sender<RadioState>,
    cmd_rx: mpsc::Receiver<CatCommand>,
    cancel: CancellationToken,
) -> (JoinHandle<Result<(), CatError>>, JoinHandle<Result<(), CatError>>) {
    // Internal channel to serialize access to the TCP connection.
    // Both poll and command tasks send requests through this channel;
    // a single connection task processes them sequentially.
    let (req_tx, req_rx) = mpsc::channel::<CatRequest>(64);

    let conn_cancel = cancel.clone();
    let conn_config = config.clone();
    let conn_handle = tokio::spawn(async move {
        run_connection(conn_config, req_rx, conn_cancel).await
    });

    let poll_cancel = cancel.clone();
    let poll_req_tx = req_tx.clone();
    let poll_handle = tokio::spawn(async move {
        run_poll(config.poll_interval, poll_req_tx, state_tx, poll_cancel).await
    });

    let cmd_cancel = cancel;
    let cmd_handle = tokio::spawn(async move {
        run_commands(cmd_rx, req_tx, cmd_cancel).await
    });

    // We return the poll and conn handles; the command task is fire-and-forget
    // but we bundle poll + conn. Simplify: return poll + conn.
    // Actually, let's return the two user-facing handles: poll and command.
    // The connection task is internal — if it exits, the others will see
    // channel closure and exit too.

    // Spawn the connection task in background (it's internal plumbing)
    tokio::spawn(async move {
        if let Err(e) = conn_handle.await {
            error!("CAT connection task panicked: {e}");
        }
    });

    (poll_handle, cmd_handle)
}

// --- internal ---

enum CatRequest {
    Poll {
        reply: tokio::sync::oneshot::Sender<Result<RadioState, CatError>>,
    },
    Command {
        raw: String,
        reply: tokio::sync::oneshot::Sender<Result<String, CatError>>,
    },
}

/// The single connection task — owns the TCP stream, processes requests sequentially.
async fn run_connection(
    config: CatConfig,
    mut rx: mpsc::Receiver<CatRequest>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let mut conn: Option<RigctldConn> = None;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(CatError::Cancelled),
            req = rx.recv() => {
                let req = match req {
                    Some(r) => r,
                    None => return Ok(()), // all senders dropped
                };

                // Ensure connected
                if conn.is_none() {
                    match RigctldConn::connect(&config.host, config.port).await {
                        Ok(c) => {
                            info!("connected to rigctld at {}:{}", config.host, config.port);
                            conn = Some(c);
                        }
                        Err(e) => {
                            warn!("rigctld connect failed: {e}");
                            // Reply with error
                            match req {
                                CatRequest::Poll { reply } => { let _ = reply.send(Err(e)); }
                                CatRequest::Command { reply, .. } => { let _ = reply.send(Err(e)); }
                            }
                            continue;
                        }
                    }
                }

                let c = conn.as_mut().unwrap();

                match req {
                    CatRequest::Poll { reply } => {
                        let result = poll_radio_state(c).await;
                        if result.is_err() {
                            conn = None; // force reconnect
                        }
                        let _ = reply.send(result);
                    }
                    CatRequest::Command { raw, reply } => {
                        let result = c.command(&raw).await;
                        if result.is_err() {
                            conn = None;
                        }
                        let _ = reply.send(result);
                    }
                }
            }
        }
    }
}

/// Poll IF; and RF; to build a RadioState.
async fn poll_radio_state(conn: &mut RigctldConn) -> Result<RadioState, CatError> {
    let if_resp = conn.command("IF;").await?;
    let (freq, mode, vfo) = parse::parse_if_response(&if_resp).ok_or_else(|| {
        CatError::BadResponse(format!("cannot parse IF response: {if_resp}"))
    })?;

    // Try to get filter bandwidth
    let filter_bw = if let Some(mode_ch) = parse::mode_char(mode) {
        let cmd = format!("RF{mode_ch};");
        match conn.command(&cmd).await {
            Ok(rf_resp) => parse::parse_rf_response(&rf_resp, mode).unwrap_or_default(),
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    Ok(RadioState {
        vfo,
        freq_hz: freq,
        mode,
        filter_bw,
        att: false,  // TODO: poll ATT state
        lp: false,   // TODO: poll LP state
        agc: AgcMode::Slow, // TODO: poll AGC state
        nr: false,
        nb: false,
        s_meter_db: -127.0, // TODO: poll S-meter
        tx: false,   // TODO: parse TX state from IF response
    })
}

/// Periodic polling task.
async fn run_poll(
    interval: Duration,
    req_tx: mpsc::Sender<CatRequest>,
    state_tx: broadcast::Sender<RadioState>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    debug!("CAT poll task started (interval={interval:?})");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(CatError::Cancelled),
            _ = ticker.tick() => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if req_tx.send(CatRequest::Poll { reply: reply_tx }).await.is_err() {
                    return Err(CatError::Disconnected);
                }

                match reply_rx.await {
                    Ok(Ok(state)) => {
                        if state_tx.send(state).is_err() {
                            debug!("no RadioState receivers");
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("poll error: {e}");
                    }
                    Err(_) => {
                        return Err(CatError::Disconnected);
                    }
                }
            }
        }
    }
}

/// Forward CatCommands from the mpsc to the connection task.
async fn run_commands(
    mut cmd_rx: mpsc::Receiver<CatCommand>,
    req_tx: mpsc::Sender<CatRequest>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    debug!("CAT command task started");

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(CatError::Cancelled),
            cmd = cmd_rx.recv() => {
                let cmd = match cmd {
                    Some(c) => c,
                    None => return Ok(()),
                };

                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if req_tx.send(CatRequest::Command {
                    raw: cmd.raw,
                    reply: reply_tx,
                }).await.is_err() {
                    return Err(CatError::Disconnected);
                }

                // We don't need the response for now — just log errors
                match reply_rx.await {
                    Ok(Ok(resp)) => {
                        debug!(response = %resp, "CAT command response");
                    }
                    Ok(Err(e)) => {
                        warn!("CAT command error: {e}");
                    }
                    Err(_) => {
                        return Err(CatError::Disconnected);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    /// Spawn a mock rigctld that responds to IF; and RF; commands.
    async fn mock_rigctld() -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];

            loop {
                let n = match socket.try_read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        continue;
                    }
                    Err(_) => break,
                };

                let cmd = String::from_utf8_lossy(&buf[..n]);
                let response = if cmd.contains("IF;") {
                    // freq=7100000, mode=USB(2) at pos 29, VFO=A(0) at pos 30
                    "IF000071000000000000000000000200;"
                } else if cmd.starts_with("RF") {
                    "RF20808;" // USB, filter index 08 = 2.4k
                } else {
                    "?;"
                };

                if socket.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        (port, handle)
    }

    #[tokio::test]
    async fn poll_from_mock_rigctld() {
        let (port, _mock) = mock_rigctld().await;

        // Give mock a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut conn = RigctldConn::connect("127.0.0.1", port).await.unwrap();
        let state = poll_radio_state(&mut conn).await.unwrap();

        assert_eq!(state.freq_hz, 7_100_000);
        assert_eq!(state.mode, efd_proto::Mode::USB);
        assert_eq!(state.vfo, efd_proto::Vfo::A);
        assert_eq!(state.filter_bw, "2.4k");
    }
}
