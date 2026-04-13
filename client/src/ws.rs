use std::sync::{Arc, Mutex};

use efd_proto::{ClientMsg, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Maximum queued messages before dropping oldest.
const MAX_QUEUE: usize = 256;

/// Maximum reconnect delay (exponential backoff caps here).
const MAX_BACKOFF_SECS: u64 = 30;

/// Start the WS connection on a background tokio thread.
/// Incoming ServerMsgs are pushed to the shared queue (polled by GTK main loop).
/// Returns an mpsc sender for outgoing ClientMsg.
pub fn start(
    url: &str,
    msg_queue: Arc<Mutex<Vec<ServerMsg>>>,
) -> mpsc::UnboundedSender<ClientMsg> {
    let (client_tx, client_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let url = url.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        rt.block_on(async move {
            run_ws(&url, msg_queue, client_rx).await;
        });
    });

    client_tx
}

async fn run_ws(
    url: &str,
    msg_queue: Arc<Mutex<Vec<ServerMsg>>>,
    mut client_rx: mpsc::UnboundedReceiver<ClientMsg>,
) {
    let mut backoff_secs: u64 = 2;

    loop {
        tracing::info!(url = %url, "WS connecting...");

        let ws = match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => {
                backoff_secs = 2; // reset on success
                ws
            }
            Err(e) => {
                tracing::warn!("WS connect failed: {e}, retrying in {backoff_secs}s");
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                continue;
            }
        };

        tracing::info!("WS connected");

        let (mut sink, mut stream) = ws.split();

        loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(frame) = frame else { break };
                    let data = match frame {
                        Ok(Message::Binary(data)) => data,
                        Ok(Message::Close(_)) => break,
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("WS read error: {e}");
                            break;
                        }
                    };

                    let msg: ServerMsg = match efd_proto::decode_msg(&data) {
                        Ok(m) => m,
                        Err(efd_proto::WireError::VersionMismatch { got, want }) => {
                            tracing::error!(
                                got, want,
                                "server wire-format mismatch — disconnecting"
                            );
                            break;
                        }
                        Err(_) => continue,
                    };

                    // Bounded queue: drop oldest if full
                    let mut q = msg_queue.lock().unwrap_or_else(|p| p.into_inner());
                    let len = q.len();
                    if len >= MAX_QUEUE {
                        q.drain(0..len / 2); // drop oldest half
                    }
                    q.push(msg);
                }
                msg = client_rx.recv() => {
                    let Some(msg) = msg else { return };
                    let bytes = match efd_proto::encode_msg(&msg) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
            }
        }

        tracing::warn!("WS disconnected, reconnecting in {backoff_secs}s");
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
}
