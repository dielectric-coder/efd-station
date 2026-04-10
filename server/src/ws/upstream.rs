use axum::extract::ws::{Message, WebSocket};
use efd_proto::{CatCommand, ClientMsg, TxAudio};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Upstream task: read WS binary frames, deserialize ClientMsg, route to mpsc channels.
pub async fn run(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    cat_tx: mpsc::Sender<CatCommand>,
    tx_audio_tx: mpsc::Sender<TxAudio>,
    cancel: CancellationToken,
) {
    let cfg = bincode::config::standard();

    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => break,
            frame = stream.next() => frame,
        };

        let Some(frame) = frame else {
            debug!("WS client disconnected (stream ended)");
            break;
        };

        let data = match frame {
            Ok(Message::Binary(data)) => data,
            Ok(Message::Close(_)) => {
                debug!("WS client sent close");
                break;
            }
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Text(_)) => {
                warn!("unexpected text frame from WS client");
                continue;
            }
            Err(e) => {
                debug!("WS read error: {e}");
                break;
            }
        };

        let msg: ClientMsg = match bincode::decode_from_slice(&data, cfg) {
            Ok((msg, _)) => msg,
            Err(e) => {
                warn!("bincode decode error: {e}");
                continue;
            }
        };

        match msg {
            ClientMsg::CatCommand(cmd) => {
                trace!(cmd = %cmd.raw, "upstream: CAT command");
                if cat_tx.send(cmd).await.is_err() {
                    warn!("CAT channel closed");
                    break;
                }
            }
            ClientMsg::TxAudio(audio) => {
                trace!(seq = audio.seq, "upstream: TX audio");
                if tx_audio_tx.send(audio).await.is_err() {
                    warn!("TX audio channel closed");
                    break;
                }
            }
            ClientMsg::Ptt(ptt) => {
                // PTT is forwarded as a CAT command for now
                let cmd = if ptt.on { "TX;" } else { "RX;" };
                trace!(ptt = ptt.on, "upstream: PTT");
                if cat_tx
                    .send(CatCommand {
                        raw: cmd.to_string(),
                    })
                    .await
                    .is_err()
                {
                    warn!("CAT channel closed");
                    break;
                }
            }
        }
    }
}
