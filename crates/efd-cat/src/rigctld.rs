use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

use crate::error::CatError;

/// Pattern to match in `/dev/serial/by-id/` for the Elad FDM-DUO CAT port.
/// The FDM-DUO exposes two USB serial interfaces; the CAT port typically
/// contains "ELAD" or "FDM-DUO" in its symlink name.
const ELAD_SERIAL_PATTERNS: &[&str] = &["ELAD", "FDM-DUO", "FDM_DUO"];

/// Configuration for rigctld process management.
#[derive(Debug, Clone)]
pub struct RigctldConfig {
    /// Serial device path, "auto", or "none".
    pub serial_device: String,
    /// Hamlib model number (-m flag). 3077 = Elad FDM-DUO.
    pub model: u32,
    /// Baud rate (-s flag).
    pub baud_rate: u32,
    /// TCP listen address for rigctld (-T flag).
    pub listen_host: String,
    /// TCP listen port for rigctld (-t flag).
    pub listen_port: u16,
}

impl Default for RigctldConfig {
    fn default() -> Self {
        Self {
            serial_device: "auto".into(),
            model: 3077,
            baud_rate: 38400,
            listen_host: "127.0.0.1".into(),
            listen_port: 4532,
        }
    }
}

/// Manages a rigctld child process.
pub struct RigctldProcess {
    child: Child,
    serial_device: String,
}

impl RigctldProcess {
    /// Resolve the serial device and spawn rigctld.
    ///
    /// - `"none"` → returns `Ok(None)` (caller should assume rigctld is external)
    /// - `"auto"` → scans `/dev/serial/by-id/` for the FDM-DUO
    /// - anything else → uses the path as-is
    pub async fn start(config: &RigctldConfig) -> Result<Option<Self>, CatError> {
        if config.serial_device == "none" {
            info!("rigctld management disabled (serial_device = none)");
            return Ok(None);
        }

        let device = if config.serial_device == "auto" {
            discover_serial_device()?.ok_or_else(|| {
                CatError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no Elad FDM-DUO serial device found in /dev/serial/by-id/",
                ))
            })?
        } else {
            let path = PathBuf::from(&config.serial_device);
            if !path.exists() {
                return Err(CatError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("serial device not found: {}", config.serial_device),
                )));
            }
            config.serial_device.clone()
        };

        info!(
            device = %device,
            model = config.model,
            baud = config.baud_rate,
            port = config.listen_port,
            "starting rigctld"
        );

        let child = Command::new("rigctld")
            .arg("-m")
            .arg(config.model.to_string())
            .arg("-r")
            .arg(&device)
            .arg("-s")
            .arg(config.baud_rate.to_string())
            .arg("-T")
            .arg(&config.listen_host)
            .arg("-t")
            .arg(config.listen_port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                CatError::Io(std::io::Error::new(
                    e.kind(),
                    format!("failed to spawn rigctld: {e}"),
                ))
            })?;

        info!(pid = child.id().unwrap_or(0), "rigctld started");

        // Give rigctld a moment to open the serial port and start listening
        tokio::time::sleep(Duration::from_millis(500)).await;

        Ok(Some(Self {
            child,
            serial_device: device,
        }))
    }

    /// Returns the serial device path being used.
    pub fn serial_device(&self) -> &str {
        &self.serial_device
    }

    /// Check if rigctld is still running.
    pub fn is_running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                warn!("rigctld exited with {status}");
                false
            }
            Err(e) => {
                error!("error checking rigctld status: {e}");
                false
            }
        }
    }

    /// Stop rigctld gracefully.
    pub async fn stop(&mut self) {
        debug!("stopping rigctld");
        if let Err(e) = self.child.kill().await {
            // Already exited — that's fine
            debug!("rigctld kill: {e}");
        }
        let _ = self.child.wait().await;
        info!("rigctld stopped");
    }
}

/// Scan `/dev/serial/by-id/` for a symlink matching the Elad FDM-DUO.
/// Returns the resolved (canonical) device path if found.
pub fn discover_serial_device() -> Result<Option<String>, CatError> {
    let by_id = Path::new("/dev/serial/by-id");
    if !by_id.exists() {
        debug!("/dev/serial/by-id/ does not exist");
        return Ok(None);
    }

    let entries = std::fs::read_dir(by_id).map_err(CatError::Io)?;

    for entry in entries {
        let entry = entry.map_err(CatError::Io)?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        for pattern in ELAD_SERIAL_PATTERNS {
            if name_str.contains(pattern) {
                let resolved = entry.path().canonicalize().map_err(CatError::Io)?;
                let device = resolved.to_string_lossy().to_string();
                info!(symlink = %name_str, device = %device, "found FDM-DUO serial device");
                return Ok(Some(device));
            }
        }
    }

    debug!("no FDM-DUO serial device found in /dev/serial/by-id/");
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_returns_none_when_no_dir() {
        // On machines without /dev/serial/by-id/, should return None, not error
        let result = discover_serial_device();
        assert!(result.is_ok());
        // May or may not find a device depending on the machine
    }
}
