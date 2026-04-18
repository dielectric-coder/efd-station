use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use efd_proto::{AgcMode, CatCommand, RadioState};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::discover;
use crate::error::CatError;
use crate::parse;
use crate::serial::{SerialPort, MAX_CAT_CMD_LEN};

/// Configuration for the CAT tasks.
#[derive(Debug, Clone)]
pub struct CatConfig {
    /// Serial device path. "auto" to discover, or explicit path.
    pub serial_device: String,
    /// Polling interval for radio state (default: 200ms).
    pub poll_interval: Duration,
}

impl Default for CatConfig {
    fn default() -> Self {
        Self {
            serial_device: "auto".into(),
            poll_interval: Duration::from_millis(200),
        }
    }
}

/// Spawn the CAT poll + command tasks.
///
/// Opens the serial port directly (no rigctld). The port is shared
/// between poll and command tasks via a Mutex.
pub fn spawn_cat_tasks(
    config: CatConfig,
    state_tx: broadcast::Sender<RadioState>,
    cmd_rx: mpsc::Receiver<CatCommand>,
    cancel: CancellationToken,
) -> (JoinHandle<Result<(), CatError>>, JoinHandle<Result<(), CatError>>) {
    // Resolve serial device
    let device = if config.serial_device == "auto" {
        match discover::discover_serial_device() {
            Ok(Some(d)) => d,
            Ok(None) => {
                error!("no FDM-DUO CAT serial device found");
                let c1 = cancel.clone();
                let h1 = tokio::spawn(async move {
                    c1.cancelled().await;
                    Err(CatError::Disconnected)
                });
                let h2 = tokio::spawn(async move {
                    cancel.cancelled().await;
                    Err(CatError::Disconnected)
                });
                return (h1, h2);
            }
            Err(e) => {
                error!("serial discovery error: {e}");
                let c1 = cancel.clone();
                let h1 = tokio::spawn(async move {
                    c1.cancelled().await;
                    Err(CatError::Disconnected)
                });
                let h2 = tokio::spawn(async move {
                    cancel.cancelled().await;
                    Err(CatError::Disconnected)
                });
                return (h1, h2);
            }
        }
    } else {
        config.serial_device.clone()
    };

    // Open serial port, shared between poll and command tasks
    let port = match SerialPort::open(&device) {
        Ok(p) => Arc::new(Mutex::new(p)),
        Err(e) => {
            error!("cannot open CAT serial port {device}: {e}");
            let c1 = cancel.clone();
            let h1 = tokio::spawn(async move {
                c1.cancelled().await;
                Err(CatError::Disconnected)
            });
            let h2 = tokio::spawn(async move {
                cancel.cancelled().await;
                Err(CatError::Disconnected)
            });
            return (h1, h2);
        }
    };

    let port2 = port.clone();
    let state_tx2 = state_tx.clone();
    let poll_cancel = cancel.clone();
    let poll_handle = tokio::task::spawn_blocking(move || {
        run_poll(port, config.poll_interval, state_tx, poll_cancel)
    });

    let cmd_cancel = cancel;
    let cmd_handle = tokio::task::spawn_blocking(move || {
        run_commands(port2, cmd_rx, state_tx2, cmd_cancel)
    });

    (poll_handle, cmd_handle)
}

/// Tracks whether we've already logged the "mutex poisoned" transition.
/// Keeps the poll task (200 ms cadence) from spamming the journal when a
/// single panic poisons the port — the cause has already been recorded
/// by the panicking task, and one line from us is enough to correlate.
static PORT_POISON_LOGGED: AtomicBool = AtomicBool::new(false);

/// Acquire the serial port mutex, recovering from poison.
///
/// The mutex is shared between the poll and command tasks; if either
/// panics while holding it (e.g. a buggy parser, an OOM), the other
/// task will see the poison. We recover the inner state and keep
/// going — the port itself is fine — but we emit exactly one error
/// log on the healthy→poisoned transition so the incident is
/// correlatable with whatever panic set it off.
fn lock_port(port: &Mutex<SerialPort>) -> std::sync::MutexGuard<'_, SerialPort> {
    port.lock().unwrap_or_else(|poisoned| {
        if !PORT_POISON_LOGGED.swap(true, Ordering::Relaxed) {
            error!(
                "CAT serial port mutex poisoned — a prior task panicked while holding \
                 the lock. Recovering inner state; later poison events will be silent."
            );
        }
        poisoned.into_inner()
    })
}

/// Poll radio state periodically.
fn run_poll(
    port: Arc<Mutex<SerialPort>>,
    interval: Duration,
    state_tx: broadcast::Sender<RadioState>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    debug!("CAT poll task started (interval={interval:?})");

    loop {
        if cancel.is_cancelled() {
            return Err(CatError::Cancelled);
        }

        std::thread::sleep(interval);

        let state = {
            let p = lock_port(&port);
            poll_radio_state(&p, &cancel)
        };

        match state {
            Ok(s) => {
                let _ = state_tx.send(s);
            }
            Err(e) => {
                warn!("CAT poll error: {e}");
            }
        }
    }
}

/// Read radio state via IF;, RF;, RI;/SM; commands.
/// Checks cancel between commands to allow quick shutdown.
fn poll_radio_state(
    port: &SerialPort,
    cancel: &CancellationToken,
) -> Result<RadioState, CatError> {
    let if_resp = port.command("IF;")?;
    let parsed = parse::parse_if_response(&if_resp).ok_or_else(|| {
        CatError::BadResponse(format!("cannot parse IF response: {if_resp}"))
    })?;

    if cancel.is_cancelled() {
        return Err(CatError::Cancelled);
    }

    let filter_bw = if let Some(mode_ch) = parse::mode_char(parsed.mode) {
        let cmd = format!("RF{mode_ch};");
        match port.command(&cmd) {
            Ok(rf_resp) => parse::parse_rf_response(&rf_resp, parsed.mode).unwrap_or_default(),
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    if cancel.is_cancelled() {
        return Err(CatError::Cancelled);
    }

    // S-meter: try RI (RSSI in dBm, more accurate), fall back to SM
    let s_meter_db = match port.command("RI;") {
        Ok(ri_resp) => parse::parse_ri_response(&ri_resp),
        Err(_) => None,
    }
    .or_else(|| {
        port.command("SM0;")
            .ok()
            .and_then(|sm_resp| parse::parse_sm_response(&sm_resp))
    })
    .unwrap_or(-127.0);

    if cancel.is_cancelled() {
        return Err(CatError::Cancelled);
    }

    // AGC threshold
    let agc_threshold = port
        .command("TH;")
        .ok()
        .and_then(|resp| parse::parse_th_response(&resp))
        .unwrap_or(0);

    if cancel.is_cancelled() {
        return Err(CatError::Cancelled);
    }

    // Optional state queries — if the radio doesn't reply (older firmware,
    // wrong dialect), each silently falls back to the documented default
    // rather than failing the whole poll.
    let att = port
        .command("RA;")
        .ok()
        .and_then(|r| parse::parse_ra_response(&r))
        .unwrap_or(false);
    let lp = port
        .command("LP;")
        .ok()
        .and_then(|r| parse::parse_lp_response(&r))
        .unwrap_or(false);
    let nr = port
        .command("NR;")
        .ok()
        .and_then(|r| parse::parse_nr_response(&r))
        .unwrap_or(false);
    let nb = port
        .command("NB;")
        .ok()
        .and_then(|r| parse::parse_nb_response(&r))
        .unwrap_or(false);
    let agc = port
        .command("GT;")
        .ok()
        .and_then(|r| parse::parse_gt_response(&r))
        .unwrap_or(AgcMode::Slow);

    Ok(RadioState {
        vfo: parsed.vfo,
        freq_hz: parsed.freq_hz,
        mode: parsed.mode,
        filter_bw,
        att,
        lp,
        agc,
        agc_threshold,
        nr,
        nb,
        s_meter_db,
        tx: parsed.tx,
    })
}

/// Forward CAT commands from WS clients to the serial port.
///
/// Coalesces rapid commands: drains the queue and only sends the last
/// command of each type (e.g., only the final FA frequency command).
/// Polls radio state once after the batch.
fn run_commands(
    port: Arc<Mutex<SerialPort>>,
    mut cmd_rx: mpsc::Receiver<CatCommand>,
    state_tx: broadcast::Sender<RadioState>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    debug!("CAT command task started");

    loop {
        if cancel.is_cancelled() {
            return Err(CatError::Cancelled);
        }

        // Wait for first command
        let first = match cmd_rx.blocking_recv() {
            Some(c) => c,
            None => return Ok(()),
        };

        // Drain any queued commands — keep only the last of each prefix
        // (e.g., last FA, last MD, last TX/RX)
        let mut last_by_prefix: Vec<CatCommand> = vec![first];
        while let Ok(cmd) = cmd_rx.try_recv() {
            // Check if this replaces a previous command with the same prefix
            let prefix = &cmd.raw[..2.min(cmd.raw.len())];
            if let Some(existing) = last_by_prefix.iter_mut().find(|c| c.raw.starts_with(prefix)) {
                *existing = cmd; // replace with newer
            } else {
                last_by_prefix.push(cmd);
            }
        }

        let p = lock_port(&port);

        for cmd in &last_by_prefix {
            if cmd.raw.len() > MAX_CAT_CMD_LEN || !cmd.raw.ends_with(';') {
                warn!(cmd = %cmd.raw, "invalid CAT command, dropping");
                continue;
            }
            match p.command(&cmd.raw) {
                Ok(resp) => {
                    debug!(cmd = %cmd.raw, response = %resp, "CAT command");
                }
                Err(e) => {
                    warn!(cmd = %cmd.raw, "CAT command error: {e}");
                }
            }
        }

        // Poll state once after the batch
        match poll_radio_state(&p, &cancel) {
            Ok(state) => {
                let _ = state_tx.send(state);
            }
            Err(CatError::Cancelled) => return Err(CatError::Cancelled),
            Err(e) => {
                debug!("post-command poll failed: {e}");
            }
        }
    }
}
