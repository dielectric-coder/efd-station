//! Hand-rolled rigctld-compatible TCP responder.
//!
//! External applications (WSJT-X, FLDIGI) connect to this as if it were
//! hamlib's `rigctld`. Two instances typically run: one fronting the
//! FDM-DUO (port 4532) and one fronting the software demod (port 4533).
//!
//! Day-one command set — `f`/`F`, `m`/`M`, `t`/`T`, `q` — covers WSJT-X's
//! minimum vocabulary. Grow on demand as other apps need more.

use std::net::SocketAddr;

use efd_proto::{CatCommand, Mode, RadioState};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::CatError;

#[derive(Debug, Clone)]
pub struct ResponderConfig {
    pub bind_addr: SocketAddr,
    /// Human label used in logs (e.g., "fdmduo-front", "demod-front").
    pub label: &'static str,
}

/// Which target the responder writes to.
#[derive(Clone)]
pub enum Backend {
    /// FDM-DUO front: set_freq / set_mode / set_ptt all go out as native CAT commands.
    Hardware {
        cat_tx: mpsc::Sender<CatCommand>,
    },
    /// Demod front: set_freq retunes the hardware (only the radio owns the LO
    /// in SDR mode); set_mode updates the demod-mode watch; set_ptt is rejected.
    Demod {
        cat_tx: mpsc::Sender<CatCommand>,
        demod_mode: watch::Sender<Option<Mode>>,
    },
}

pub fn spawn_responder(
    cfg: ResponderConfig,
    backend: Backend,
    state_rx: watch::Receiver<Option<RadioState>>,
    cancel: CancellationToken,
) -> JoinHandle<Result<(), CatError>> {
    tokio::spawn(async move { run(cfg, backend, state_rx, cancel).await })
}

async fn run(
    cfg: ResponderConfig,
    backend: Backend,
    state_rx: watch::Receiver<Option<RadioState>>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let listener = TcpListener::bind(cfg.bind_addr).await?;
    info!(bind = %cfg.bind_addr, label = cfg.label, "rigctld responder listening");

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!(label = cfg.label, "responder cancelled");
                return Err(CatError::Cancelled);
            }
            accept = listener.accept() => {
                let (stream, peer) = match accept {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!(label = cfg.label, "accept error: {e}");
                        continue;
                    }
                };
                debug!(label = cfg.label, %peer, "rigctld client connected");
                let backend = backend.clone();
                let state_rx = state_rx.clone();
                let conn_cancel = cancel.clone();
                let label = cfg.label;
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, backend, state_rx, conn_cancel).await {
                        debug!(label, %peer, "rigctld client: {e}");
                    }
                });
            }
        }
    }
}

async fn handle_conn(
    stream: TcpStream,
    backend: Backend,
    state_rx: watch::Receiver<Option<RadioState>>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            line = lines.next_line() => {
                let line = match line? {
                    Some(l) => l,
                    None => return Ok(()),
                };
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }

                match handle_command(trimmed, &backend, &state_rx).await {
                    Reply::Response(s) => w.write_all(s.as_bytes()).await?,
                    Reply::Close => return Ok(()),
                }
            }
        }
    }
}

enum Reply {
    Response(String),
    Close,
}

async fn handle_command(
    line: &str,
    backend: &Backend,
    state_rx: &watch::Receiver<Option<RadioState>>,
) -> Reply {
    let mut parts = line.split_ascii_whitespace();
    let cmd = match parts.next() {
        Some(c) => c,
        None => return Reply::Response("RPRT -11\n".into()),
    };

    match cmd {
        "f" => {
            let hz = state_rx.borrow().as_ref().map(|s| s.freq_hz).unwrap_or(0);
            Reply::Response(format!("{hz}\n"))
        }
        "F" => {
            let Some(hz) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
                return Reply::Response("RPRT -1\n".into());
            };
            backend.set_freq(hz).await;
            Reply::Response("RPRT 0\n".into())
        }
        "m" => {
            let mode = current_mode(backend, state_rx);
            // Passband not tracked per mode yet — return 0 ("default").
            Reply::Response(format!("{}\n0\n", mode_to_str(mode)))
        }
        "M" => {
            let Some(mode_str) = parts.next() else {
                return Reply::Response("RPRT -1\n".into());
            };
            let Some(mode) = str_to_mode(mode_str) else {
                return Reply::Response("RPRT -11\n".into());
            };
            // Passband argument parsed but not applied (no per-mode passband
            // control surface today).
            let _passband_hz = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            backend.set_mode(mode).await;
            Reply::Response("RPRT 0\n".into())
        }
        "t" => {
            let tx = state_rx.borrow().as_ref().map(|s| s.tx).unwrap_or(false);
            Reply::Response(format!("{}\n", if tx { 1 } else { 0 }))
        }
        "T" => {
            let Some(arg) = parts.next() else {
                return Reply::Response("RPRT -1\n".into());
            };
            let on = arg == "1";
            if !backend.set_ptt(on).await {
                return Reply::Response("RPRT -11\n".into());
            }
            Reply::Response("RPRT 0\n".into())
        }
        "q" | "Q" => Reply::Close,
        _ => Reply::Response("RPRT -11\n".into()),
    }
}

fn current_mode(
    backend: &Backend,
    state_rx: &watch::Receiver<Option<RadioState>>,
) -> Mode {
    match backend {
        Backend::Demod { demod_mode, .. } => demod_mode.borrow().unwrap_or_else(|| {
            state_rx
                .borrow()
                .as_ref()
                .map(|s| s.mode)
                .unwrap_or(Mode::Unknown)
        }),
        Backend::Hardware { .. } => state_rx
            .borrow()
            .as_ref()
            .map(|s| s.mode)
            .unwrap_or(Mode::Unknown),
    }
}

impl Backend {
    async fn set_freq(&self, hz: u64) {
        let cat = match self {
            Backend::Hardware { cat_tx } | Backend::Demod { cat_tx, .. } => cat_tx,
        };
        let _ = cat
            .send(CatCommand {
                raw: format!("FA{:011};", hz),
            })
            .await;
    }

    async fn set_mode(&self, mode: Mode) {
        match self {
            Backend::Hardware { cat_tx } => {
                if let Some(raw) = mode_to_cat(mode) {
                    let _ = cat_tx.send(CatCommand { raw }).await;
                }
            }
            Backend::Demod { demod_mode, .. } => {
                let _ = demod_mode.send(Some(mode));
            }
        }
    }

    /// Returns false when the backend does not support PTT (demod front).
    async fn set_ptt(&self, on: bool) -> bool {
        match self {
            Backend::Hardware { cat_tx } => {
                let raw = if on { "TX;".to_string() } else { "RX;".to_string() };
                let _ = cat_tx.send(CatCommand { raw }).await;
                true
            }
            Backend::Demod { .. } => false,
        }
    }
}

fn mode_to_str(mode: Mode) -> &'static str {
    match mode {
        Mode::USB => "USB",
        Mode::LSB => "LSB",
        Mode::CW => "CW",
        Mode::CWR => "CWR",
        Mode::AM => "AM",
        Mode::FM => "FM",
        Mode::Unknown => "USB",
    }
}

fn str_to_mode(s: &str) -> Option<Mode> {
    Some(match s {
        "USB" | "PKTUSB" => Mode::USB,
        "LSB" | "PKTLSB" => Mode::LSB,
        "CW" => Mode::CW,
        "CWR" => Mode::CWR,
        "AM" => Mode::AM,
        "FM" | "PKTFM" | "FMN" | "WFM" => Mode::FM,
        _ => return None,
    })
}

fn mode_to_cat(mode: Mode) -> Option<String> {
    let digit = match mode {
        Mode::LSB => '1',
        Mode::USB => '2',
        Mode::CW => '3',
        Mode::FM => '4',
        Mode::AM => '5',
        Mode::CWR => '7',
        Mode::Unknown => return None,
    };
    Some(format!("MD{digit};"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_roundtrip() {
        for m in [Mode::USB, Mode::LSB, Mode::CW, Mode::CWR, Mode::AM, Mode::FM] {
            let s = mode_to_str(m);
            assert_eq!(str_to_mode(s), Some(m));
        }
    }

    #[test]
    fn pkt_variants_map_to_base() {
        assert_eq!(str_to_mode("PKTUSB"), Some(Mode::USB));
        assert_eq!(str_to_mode("PKTLSB"), Some(Mode::LSB));
        assert_eq!(str_to_mode("PKTFM"), Some(Mode::FM));
    }

    #[test]
    fn unknown_mode_rejected() {
        assert_eq!(str_to_mode("FOO"), None);
    }
}
