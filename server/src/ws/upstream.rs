use axum::extract::ws::{Message, WebSocket};
use efd_proto::{AudioSource, CatCommand, ClientMsg, Mode, TxAudio};
use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace, warn};

/// Maximum WS frame size we'll decode (4 KB — plenty for any valid message).
const MAX_WS_FRAME: usize = 4096;

/// Maximum CAT command length from a client.
const MAX_CAT_CMD_LEN: usize = 64;

/// Maximum Opus frame size from a client (typical Opus frame < 500 bytes).
const MAX_TX_AUDIO_LEN: usize = 2048;

/// Upstream task: read WS binary frames, deserialize ClientMsg, route to mpsc channels.
pub async fn run(
    mut stream: futures_util::stream::SplitStream<WebSocket>,
    cat_tx: mpsc::Sender<CatCommand>,
    tx_audio_tx: mpsc::Sender<TxAudio>,
    demod_mode_tx: watch::Sender<Option<Mode>>,
    audio_source_tx: watch::Sender<AudioSource>,
    cancel: CancellationToken,
) {
    let cfg = bincode::config::standard().with_limit::<MAX_WS_FRAME>();

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
            Ok(Message::Binary(data)) => {
                if data.len() > MAX_WS_FRAME {
                    warn!(len = data.len(), "WS frame too large, dropping");
                    continue;
                }
                data
            }
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
                if !validate_cat_command(&cmd.raw) {
                    warn!(cmd = %cmd.raw, "invalid CAT command rejected");
                    continue;
                }
                trace!(cmd = %cmd.raw, "upstream: CAT command");
                if cat_tx.send(cmd).await.is_err() {
                    warn!("CAT channel closed");
                    break;
                }
            }
            ClientMsg::TxAudio(audio) => {
                if audio.opus_data.len() > MAX_TX_AUDIO_LEN {
                    warn!(len = audio.opus_data.len(), "TX audio frame too large");
                    continue;
                }
                trace!(seq = audio.seq, "upstream: TX audio");
                if tx_audio_tx.send(audio).await.is_err() {
                    warn!("TX audio channel closed");
                    break;
                }
            }
            ClientMsg::Ptt(ptt) => {
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
            ClientMsg::SetAudioSource(src) => {
                debug!(?src, "upstream: audio source selection");
                let _ = audio_source_tx.send(src);
            }
            ClientMsg::SetDemodMode(mode) => {
                debug!(?mode, "upstream: demod mode override");
                let _ = demod_mode_tx.send(mode);
            }
        }
    }
}

/// Validate a CAT command from a WS client.
/// Only allows printable ASCII, must end with ';', length limited.
fn validate_cat_command(cmd: &str) -> bool {
    if cmd.is_empty() || cmd.len() > MAX_CAT_CMD_LEN {
        return false;
    }
    if !cmd.ends_with(';') {
        return false;
    }
    // Only printable ASCII (0x20..=0x7E)
    cmd.bytes().all(|b| (0x20..=0x7E).contains(&b))
}
