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
    flip_spectrum_tx: watch::Sender<bool>,
    cancel: CancellationToken,
) {
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

        let msg: ClientMsg = match efd_proto::decode_msg(&data) {
            Ok(m) => m,
            Err(efd_proto::WireError::VersionMismatch { got, want }) => {
                warn!(
                    got,
                    want,
                    "WS client wire-format mismatch — disconnecting"
                );
                break;
            }
            Err(e) => {
                warn!("WS decode error: {e}");
                continue;
            }
        };

        match msg {
            ClientMsg::CatCommand(cmd) => {
                if let Err(reason) = validate_cat_command(&cmd.raw) {
                    warn!(cmd = %cmd.raw, reason, "invalid CAT command rejected");
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
            ClientMsg::SetDrmFlipSpectrum(flip) => {
                debug!(flip, "upstream: DRM flip_spectrum toggle");
                let _ = flip_spectrum_tx.send(flip);
            }
        }
    }
}

/// Allowlist of two-letter CAT prefixes accepted from WS clients. Keeps
/// untrusted input on the rails — anything outside this set is rejected
/// before it reaches the radio. Grow as the UI gains controls.
const ALLOWED_CAT_PREFIXES: &[&str] = &[
    "FA", "FB", // VFO A/B frequency
    "MD",       // mode
    "RF",       // filter bandwidth (RF<mode><idx>)
    "RA",       // attenuator
    "LP",       // 50 MHz low-pass filter
    "GT", "TH", // AGC mode + threshold
    "NR", "NB", // noise reduction / blanker
    "RT", "XT", // RIT / XIT enable
    "RC",       // RIT clear
    "RU", "RD", // RIT up / down
    "TX", "RX", // PTT on / off
    "IF", "RI", "SM", // poll commands (IF status, RSSI, S-meter)
    "FR", "FT", // RX/TX VFO selection
    "AI",       // auto-info
];

/// Validate a CAT command from a WS client.
///
/// The radio's CAT dialect is `XX[payload];` where `XX` is two ASCII
/// uppercase letters. We:
///   - cap length;
///   - require the trailing `;`;
///   - require the prefix to be uppercase ASCII letters and on the
///     allowlist above (so an attacker can't poke at undocumented or
///     destructive commands by guessing prefixes);
///   - restrict the payload to printable ASCII without `;` (which would
///     allow stuffing a second command into one frame).
fn validate_cat_command(cmd: &str) -> Result<(), &'static str> {
    if cmd.len() < 3 || cmd.len() > MAX_CAT_CMD_LEN {
        return Err("length out of range");
    }
    if !cmd.ends_with(';') {
        return Err("missing trailing ';'");
    }
    let bytes = cmd.as_bytes();
    if !bytes[0].is_ascii_uppercase() || !bytes[1].is_ascii_uppercase() {
        return Err("prefix not ASCII uppercase");
    }
    let prefix = &cmd[..2];
    if !ALLOWED_CAT_PREFIXES.contains(&prefix) {
        return Err("prefix not on allowlist");
    }
    // Payload between the prefix and the trailing ';' must be
    // printable-ASCII and free of an embedded ';' (which would let a
    // client smuggle a second command).
    let payload = &cmd[2..cmd.len() - 1];
    if !payload
        .bytes()
        .all(|b| (0x20..=0x7E).contains(&b) && b != b';')
    {
        return Err("payload has non-printable or embedded ';'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_well_formed_commands() {
        assert!(validate_cat_command("FA00007100000;").is_ok());
        assert!(validate_cat_command("MD2;").is_ok());
        assert!(validate_cat_command("TX;").is_ok());
        assert!(validate_cat_command("RX;").is_ok());
        assert!(validate_cat_command("IF;").is_ok());
    }

    #[test]
    fn rejects_unknown_prefix() {
        assert_eq!(validate_cat_command("ZZ00;"), Err("prefix not on allowlist"));
    }

    #[test]
    fn rejects_lowercase_prefix() {
        assert_eq!(validate_cat_command("fa12345;"), Err("prefix not ASCII uppercase"));
    }

    #[test]
    fn rejects_missing_terminator() {
        assert_eq!(validate_cat_command("FA00007100000"), Err("missing trailing ';'"));
    }

    #[test]
    fn rejects_embedded_semicolon() {
        assert_eq!(
            validate_cat_command("FA;TX;"),
            Err("payload has non-printable or embedded ';'")
        );
    }

    #[test]
    fn rejects_oversize() {
        let big = format!("FA{};", "0".repeat(MAX_CAT_CMD_LEN));
        assert_eq!(validate_cat_command(&big), Err("length out of range"));
    }

    #[test]
    fn rejects_too_short() {
        assert_eq!(validate_cat_command(";"), Err("length out of range"));
        assert_eq!(validate_cat_command("F;"), Err("length out of range"));
    }
}
