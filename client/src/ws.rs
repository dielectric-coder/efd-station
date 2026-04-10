use std::sync::{Arc, Mutex};

use efd_proto::{ClientMsg, ServerMsg};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

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
    let cfg = bincode::config::standard();

    loop {
        tracing::info!(url = %url, "WS connecting...");

        let ws = match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => ws,
            Err(e) => {
                tracing::warn!("WS connect failed: {e}, retrying in 2s");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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

                    let msg: ServerMsg = match bincode::decode_from_slice(&data, cfg) {
                        Ok((msg, _)) => msg,
                        Err(_) => continue,
                    };

                    msg_queue.lock().unwrap().push(msg);
                }
                msg = client_rx.recv() => {
                    let Some(msg) = msg else { return };
                    let bytes = match bincode::encode_to_vec(&msg, cfg) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
            }
        }

        tracing::warn!("WS disconnected, reconnecting in 2s");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}
