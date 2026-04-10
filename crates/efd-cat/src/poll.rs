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
use crate::serial::SerialPort;

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
    let poll_cancel = cancel.clone();
    let poll_handle = tokio::task::spawn_blocking(move || {
        run_poll(port, config.poll_interval, state_tx, poll_cancel)
    });

    let cmd_cancel = cancel;
    let cmd_handle = tokio::task::spawn_blocking(move || {
        run_commands(port2, cmd_rx, cmd_cancel)
    });

    (poll_handle, cmd_handle)
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
            let p = port.lock().unwrap();
            poll_radio_state(&p)
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

/// Read radio state via IF; and RF; commands.
fn poll_radio_state(port: &SerialPort) -> Result<RadioState, CatError> {
    let if_resp = port.command("IF;")?;
    let (freq, mode, vfo) = parse::parse_if_response(&if_resp).ok_or_else(|| {
        CatError::BadResponse(format!("cannot parse IF response: {if_resp}"))
    })?;

    let filter_bw = if let Some(mode_ch) = parse::mode_char(mode) {
        let cmd = format!("RF{mode_ch};");
        match port.command(&cmd) {
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
        att: false,
        lp: false,
        agc: AgcMode::Slow,
        nr: false,
        nb: false,
        s_meter_db: -127.0,
        tx: false,
    })
}

/// Forward CAT commands from WS clients to the serial port.
fn run_commands(
    port: Arc<Mutex<SerialPort>>,
    mut cmd_rx: mpsc::Receiver<CatCommand>,
    cancel: CancellationToken,
) -> Result<(), CatError> {
    debug!("CAT command task started");

    loop {
        if cancel.is_cancelled() {
            return Err(CatError::Cancelled);
        }

        // blocking_recv with a timeout check
        let cmd = match cmd_rx.blocking_recv() {
            Some(c) => c,
            None => return Ok(()),
        };

        let p = port.lock().unwrap();
        match p.command(&cmd.raw) {
            Ok(resp) => {
                debug!(cmd = %cmd.raw, response = %resp, "CAT command");
            }
            Err(e) => {
                warn!(cmd = %cmd.raw, "CAT command error: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Serial port tests require hardware — integration tests only
}
