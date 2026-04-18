//! Hand-rolled rigctld-compatible TCP responder.
//!
//! External applications (WSJT-X, FLDIGI) connect to this as if it were
//! hamlib's `rigctld`. Two instances typically run: one fronting the
//! FDM-DUO (port 4532) and one fronting the software demod (port 4533).
//!
//! Day-one command set — `f`/`F`, `m`/`M`, `t`/`T`, `q` — covers WSJT-X's
//! minimum vocabulary. Grow on demand as other apps need more.

use std::net::SocketAddr;
use std::sync::Arc;

use efd_proto::{CatCommand, Mode, RadioState, Vfo};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Hard cap on concurrent clients per responder instance. Real rigctld
/// clients (WSJT-X, FLDIGI) use one connection each and there are usually
/// 1–2 of them total; this caps the fd/memory blast radius if something
/// local starts opening connections in a loop.
const MAX_CONCURRENT_CONNS: usize = 16;

/// Sanity bounds for rigctld `F` (set-frequency). The native CAT format
/// is `FA<11-digit-hz>;`, so values ≥ 100 GHz would produce malformed
/// frames. We also reject absurdly low values. This is a *wire sanity*
/// check, not a per-device capability check — the radio's firmware
/// still filters to its own band (e.g. 9 kHz – 54 MHz on the FDM-DUO)
/// and a too-high-for-this-device value just comes back as a radio
/// error, not a jammed command queue.
const MIN_FREQ_HZ: u64 = 1_000;
const MAX_FREQ_HZ: u64 = 99_999_999_999;

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
    run_with_listener(listener, cfg.label, backend, state_rx, cancel).await
}

pub(crate) async fn run_with_listener(
    listener: TcpListener,
    label: &'static str,
    backend: Backend,
    state_rx: watch::Receiver<Option<RadioState>>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNS));
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                debug!(label, "responder cancelled");
                return Err(CatError::Cancelled);
            }
            accept = listener.accept() => {
                let (stream, peer) = match accept {
                    Ok(pair) => pair,
                    Err(e) => {
                        warn!(label, "accept error: {e}");
                        continue;
                    }
                };
                let permit = match conn_sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        warn!(label, %peer, "connection cap reached, rejecting");
                        drop(stream);
                        continue;
                    }
                };
                debug!(label, %peer, "rigctld client connected");
                let backend = backend.clone();
                let state_rx = state_rx.clone();
                let conn_cancel = cancel.clone();
                tokio::spawn(async move {
                    let _permit = permit; // released when the task exits
                    if let Err(e) = handle_conn(stream, backend, state_rx, conn_cancel).await {
                        debug!(label, %peer, "rigctld client: {e}");
                    }
                });
            }
        }
    }
}

/// Cap per-line input from a rigctld client. A well-formed command is
/// a handful of bytes; real-world ones top out around 64 bytes. 4 KiB
/// is generous and prevents an abusive client from forcing unbounded
/// allocation through `BufReader`'s growing internal buffer.
const MAX_LINE_BYTES: usize = 4096;

async fn handle_conn(
    stream: TcpStream,
    backend: Backend,
    state_rx: watch::Receiver<Option<RadioState>>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line_buf = Vec::with_capacity(128);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            res = read_line_bounded(&mut reader, &mut line_buf, MAX_LINE_BYTES) => {
                match res? {
                    None => return Ok(()),                // EOF
                    Some(true) => {}                      // got a line
                    Some(false) => {
                        // Oversized line — discard and disconnect.
                        return Ok(());
                    }
                }
                // rigctld is a line protocol; non-UTF-8 input is always
                // garbage. Skip the line entirely instead of silently
                // turning it into "" — that just made bad input look like
                // an empty line and obscured debugging.
                let line = match std::str::from_utf8(&line_buf) {
                    Ok(s) => s,
                    Err(_) => {
                        debug!("rigctld: dropping non-UTF-8 line");
                        continue;
                    }
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

/// Read one `\n`-terminated line into `buf` with a hard byte cap.
/// Returns:
/// - `None` on clean EOF (no bytes read)
/// - `Some(true)` when a line was read (terminator consumed, not included in `buf`)
/// - `Some(false)` when the line exceeded `max_bytes` — remaining bytes on
///   the stream are not drained; the caller should close the connection.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> Result<Option<bool>, CatError> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() { None } else { Some(true) });
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > max_bytes {
                return Ok(Some(false));
            }
            buf.extend_from_slice(&available[..pos]);
            let consumed = pos + 1;
            reader.consume(consumed);
            return Ok(Some(true));
        }
        if buf.len() + available.len() > max_bytes {
            return Ok(Some(false));
        }
        buf.extend_from_slice(available);
        let n = available.len();
        reader.consume(n);
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
            if !(MIN_FREQ_HZ..=MAX_FREQ_HZ).contains(&hz) {
                warn!(hz, "rigctld F: frequency out of sanity range");
                // RPRT -11 = invalid parameter, per hamlib convention.
                return Reply::Response("RPRT -11\n".into());
            }
            let vfo = state_rx
                .borrow()
                .as_ref()
                .map(|s| s.vfo)
                .unwrap_or(Vfo::A);
            if !backend.set_freq(hz, vfo).await {
                return Reply::Response("RPRT -6\n".into());
            }
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
            if !backend.set_mode(mode).await {
                return Reply::Response("RPRT -6\n".into());
            }
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
            match backend.set_ptt(on).await {
                PttResult::Ok => Reply::Response("RPRT 0\n".into()),
                PttResult::Unsupported => Reply::Response("RPRT -11\n".into()),
                PttResult::IoFailure => Reply::Response("RPRT -6\n".into()),
            }
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

enum PttResult {
    Ok,
    Unsupported,
    IoFailure,
}

impl Backend {
    /// Tune the active VFO on the radio. Both Hardware and Demod backends
    /// route through the native CAT channel — only the radio owns the LO.
    /// Returns false if the CAT channel is closed.
    async fn set_freq(&self, hz: u64, vfo: Vfo) -> bool {
        let cat = match self {
            Backend::Hardware { cat_tx } | Backend::Demod { cat_tx, .. } => cat_tx,
        };
        let prefix = match vfo {
            Vfo::A => "FA",
            Vfo::B => "FB",
        };
        cat.send(CatCommand {
            raw: format!("{prefix}{:011};", hz),
        })
        .await
        .is_ok()
    }

    /// Returns false if the underlying send failed (CAT channel closed,
    /// or an internally unreachable invalid mode).
    async fn set_mode(&self, mode: Mode) -> bool {
        match self {
            Backend::Hardware { cat_tx } => match mode_to_cat(mode) {
                Some(raw) => cat_tx.send(CatCommand { raw }).await.is_ok(),
                None => false,
            },
            Backend::Demod { demod_mode, .. } => {
                // Skip the send if the demod is already in this mode to avoid
                // waking every downstream watch consumer for a no-op.
                if demod_mode.borrow().as_ref() != Some(&mode) {
                    let _ = demod_mode.send(Some(mode));
                }
                true
            }
        }
    }

    async fn set_ptt(&self, on: bool) -> PttResult {
        match self {
            Backend::Hardware { cat_tx } => {
                let raw = if on { "TX;".to_string() } else { "RX;".to_string() };
                if cat_tx.send(CatCommand { raw }).await.is_ok() {
                    PttResult::Ok
                } else {
                    PttResult::IoFailure
                }
            }
            Backend::Demod { .. } => PttResult::Unsupported,
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
        // Software-only AM-family modes and DRM are not rigctld standard
        // modes; report as AM so hamlib clients see something coherent.
        Mode::DRM | Mode::SAM | Mode::SAMU | Mode::SAML | Mode::DSB => "AM",
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
        // DRM and the software-only AM-family modes leave the radio in
        // AM; the demod decision is handled via demod_mode_tx.
        Mode::AM | Mode::DRM | Mode::SAM | Mode::SAMU | Mode::SAML | Mode::DSB => '5',
        Mode::CWR => '7',
        Mode::Unknown => return None,
    };
    Some(format!("MD{digit};"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use efd_proto::{AgcMode, Vfo};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

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

    fn fake_state(freq_hz: u64, mode: Mode, tx: bool) -> RadioState {
        RadioState {
            vfo: Vfo::A,
            freq_hz,
            mode,
            filter_bw: String::new(),
            filter_bw_hz: None,
            att: false,
            lp: false,
            agc: AgcMode::Slow,
            agc_threshold: 0,
            nr: false,
            nb: false,
            s_meter_db: -73.0,
            tx,
            rit_hz: 0,
            rit_on: false,
            xit_hz: 0,
            xit_on: false,
            if_offset_hz: 0,
            snr_db: None,
        }
    }

    struct Harness {
        _cancel: CancellationToken,
        addr: std::net::SocketAddr,
        cat_rx: mpsc::Receiver<CatCommand>,
        demod_mode_rx: Option<watch::Receiver<Option<Mode>>>,
    }

    async fn spawn_hw(state: Option<RadioState>) -> Harness {
        let (cat_tx, cat_rx) = mpsc::channel::<CatCommand>(16);
        let (_state_tx, state_rx) = watch::channel(state);
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Backend::Hardware { cat_tx };
        let c = cancel.clone();
        tokio::spawn(async move {
            let _ = run_with_listener(listener, "test-hw", backend, state_rx, c).await;
        });
        Harness {
            _cancel: cancel,
            addr,
            cat_rx,
            demod_mode_rx: None,
        }
    }

    async fn spawn_demod(state: Option<RadioState>) -> Harness {
        let (cat_tx, cat_rx) = mpsc::channel::<CatCommand>(16);
        let (_state_tx, state_rx) = watch::channel(state);
        let (demod_mode_tx, demod_mode_rx) = watch::channel::<Option<Mode>>(None);
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Backend::Demod {
            cat_tx,
            demod_mode: demod_mode_tx,
        };
        let c = cancel.clone();
        tokio::spawn(async move {
            let _ = run_with_listener(listener, "test-demod", backend, state_rx, c).await;
        });
        Harness {
            _cancel: cancel,
            addr,
            cat_rx,
            demod_mode_rx: Some(demod_mode_rx),
        }
    }

    async fn send_recv(addr: std::net::SocketAddr, req: &str) -> String {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(b"q\n").await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), s.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[tokio::test]
    async fn get_freq_reports_state() {
        let h = spawn_hw(Some(fake_state(14_074_000, Mode::USB, false))).await;
        assert_eq!(send_recv(h.addr, "f\n").await, "14074000\n");
    }

    #[tokio::test]
    async fn get_freq_zero_when_no_state() {
        let h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "f\n").await, "0\n");
    }

    #[tokio::test]
    async fn set_freq_emits_cat_command() {
        let mut h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "F 14074000\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "FA00014074000;");
    }

    #[tokio::test]
    async fn set_freq_uses_vfo_b_when_state_says_b() {
        let mut state = fake_state(0, Mode::USB, false);
        state.vfo = Vfo::B;
        let mut h = spawn_hw(Some(state)).await;
        assert_eq!(send_recv(h.addr, "F 7074000\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "FB00007074000;");
    }

    #[tokio::test]
    async fn set_freq_returns_rprt_minus_6_when_cat_closed() {
        // Drop cat_rx before issuing the command so cat_tx.send fails.
        let (cat_tx, cat_rx) = mpsc::channel::<CatCommand>(16);
        let (_state_tx, state_rx) = watch::channel::<Option<RadioState>>(None);
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Backend::Hardware { cat_tx };
        tokio::spawn(async move {
            let _ = run_with_listener(listener, "test-hw", backend, state_rx, cancel).await;
        });
        drop(cat_rx);
        assert_eq!(send_recv(addr, "F 14074000\n").await, "RPRT -6\n");
    }

    #[tokio::test]
    async fn set_freq_missing_arg_reports_error() {
        let h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "F\n").await, "RPRT -1\n");
    }

    #[tokio::test]
    async fn get_mode_reports_state() {
        let h = spawn_hw(Some(fake_state(0, Mode::LSB, false))).await;
        assert_eq!(send_recv(h.addr, "m\n").await, "LSB\n0\n");
    }

    #[tokio::test]
    async fn set_mode_on_hardware_emits_cat_command() {
        let mut h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "M CW 500\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "MD3;");
    }

    #[tokio::test]
    async fn set_mode_pktusb_maps_to_usb() {
        let mut h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "M PKTUSB 3000\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "MD2;");
    }

    #[tokio::test]
    async fn set_mode_unknown_reports_error() {
        let h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "M BOGUS 0\n").await, "RPRT -11\n");
    }

    #[tokio::test]
    async fn get_ptt_reports_state() {
        let h_rx = spawn_hw(Some(fake_state(0, Mode::USB, false))).await;
        assert_eq!(send_recv(h_rx.addr, "t\n").await, "0\n");
        let h_tx = spawn_hw(Some(fake_state(0, Mode::USB, true))).await;
        assert_eq!(send_recv(h_tx.addr, "t\n").await, "1\n");
    }

    #[tokio::test]
    async fn set_ptt_on_hardware_emits_tx_command() {
        let mut h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "T 1\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "TX;");
    }

    #[tokio::test]
    async fn set_ptt_off_hardware_emits_rx_command() {
        let mut h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "T 0\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "RX;");
    }

    #[tokio::test]
    async fn set_ptt_on_demod_backend_rejected() {
        let h = spawn_demod(None).await;
        assert_eq!(send_recv(h.addr, "T 1\n").await, "RPRT -11\n");
    }

    #[tokio::test]
    async fn demod_set_mode_updates_watch_not_cat() {
        let mut h = spawn_demod(None).await;
        assert_eq!(send_recv(h.addr, "M CW 500\n").await, "RPRT 0\n");
        // cat_rx should stay empty
        assert!(tokio::time::timeout(Duration::from_millis(50), h.cat_rx.recv()).await.is_err());
        // watch should have the new mode
        let rx = h.demod_mode_rx.as_mut().unwrap();
        rx.changed().await.unwrap();
        assert_eq!(*rx.borrow(), Some(Mode::CW));
    }

    #[tokio::test]
    async fn demod_set_mode_skips_watch_when_unchanged() {
        // Pre-seed the demod watch at CW. Setting CW again should not fire.
        let (cat_tx, _cat_rx) = mpsc::channel::<CatCommand>(16);
        let (_state_tx, state_rx) = watch::channel::<Option<RadioState>>(None);
        let (demod_mode_tx, mut demod_mode_rx) = watch::channel(Some(Mode::CW));
        demod_mode_rx.mark_unchanged();
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Backend::Demod {
            cat_tx,
            demod_mode: demod_mode_tx,
        };
        tokio::spawn(async move {
            let _ = run_with_listener(listener, "test-demod", backend, state_rx, cancel).await;
        });
        assert_eq!(send_recv(addr, "M CW 500\n").await, "RPRT 0\n");
        // demod_mode_rx.changed() should NOT fire within a small window.
        assert!(tokio::time::timeout(Duration::from_millis(50), demod_mode_rx.changed()).await.is_err());
    }

    #[tokio::test]
    async fn demod_set_freq_still_retunes_hardware() {
        let mut h = spawn_demod(None).await;
        assert_eq!(send_recv(h.addr, "F 7074000\n").await, "RPRT 0\n");
        let cmd = h.cat_rx.recv().await.unwrap();
        assert_eq!(cmd.raw, "FA00007074000;");
    }

    #[tokio::test]
    async fn demod_get_mode_prefers_demod_watch() {
        // State says USB, but demod watch overrides to CW.
        let (cat_tx, _cat_rx) = mpsc::channel::<CatCommand>(16);
        let (_state_tx, state_rx) =
            watch::channel(Some(fake_state(0, Mode::USB, false)));
        let (demod_mode_tx, _demod_mode_rx) = watch::channel(Some(Mode::CW));
        let cancel = CancellationToken::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let backend = Backend::Demod {
            cat_tx,
            demod_mode: demod_mode_tx,
        };
        tokio::spawn(async move {
            let _ = run_with_listener(listener, "test-demod", backend, state_rx, cancel).await;
        });
        assert_eq!(send_recv(addr, "m\n").await, "CW\n0\n");
    }

    #[tokio::test]
    async fn unknown_command_returns_rprt_minus_11() {
        let h = spawn_hw(None).await;
        assert_eq!(send_recv(h.addr, "dump_state\n").await, "RPRT -11\n");
    }

    #[tokio::test]
    async fn quit_closes_connection_cleanly() {
        let h = spawn_hw(None).await;
        let mut s = TcpStream::connect(h.addr).await.unwrap();
        s.write_all(b"q\n").await.unwrap();
        let mut buf = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(1), s.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn connection_cap_rejects_beyond_max() {
        let h = spawn_hw(None).await;
        // Open MAX_CONCURRENT_CONNS idle connections and hold them.
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_CONNS {
            let s = TcpStream::connect(h.addr).await.unwrap();
            held.push(s);
        }
        // Give the server a moment to register the connections.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The next connection should be accepted at the TCP layer but immediately
        // dropped by the responder with no response.
        let mut extra = TcpStream::connect(h.addr).await.unwrap();
        extra.write_all(b"f\n").await.ok();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), extra.read_to_end(&mut buf))
            .await
            .unwrap()
            .ok();
        assert!(buf.is_empty(), "rejected client should get no bytes");
    }

    #[tokio::test]
    async fn oversized_line_disconnects_no_oom() {
        let h = spawn_hw(None).await;
        let mut s = TcpStream::connect(h.addr).await.unwrap();
        // Send a line much larger than MAX_LINE_BYTES without a newline.
        let junk = vec![b'A'; MAX_LINE_BYTES * 2];
        // Writes may succeed (small kernel buffer) or fail once the server
        // closes the socket; either is acceptable — what matters is no OOM
        // and clean close.
        let _ = s.write_all(&junk).await;
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), s.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
        // Server should have closed without replying to the (unparseable) input.
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn multiple_commands_one_connection() {
        let h = spawn_hw(Some(fake_state(14_074_000, Mode::USB, false))).await;
        let mut s = TcpStream::connect(h.addr).await.unwrap();
        s.write_all(b"f\nt\nq\n").await.unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), s.read_to_end(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "14074000\n0\n");
    }
}
