use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};
use tracing::{debug, error, info, warn};

use crate::error::CatError;

/// Pattern to match in `/dev/serial/by-id/` for the Elad FDM-DUO CAT port.
const ELAD_SERIAL_PATTERNS: &[&str] = &["ELAD", "FDM-DUO", "FDM_DUO"];

/// Elad USB vendor/product IDs for sysfs matching.
const ELAD_VID: &str = "1721";
const ELAD_PID: &str = "061a";

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

/// Auto-discover the FDM-DUO CAT serial port.
///
/// Strategy:
/// 1. Scan `/dev/serial/by-id/` for symlinks matching ELAD/FDM-DUO
/// 2. Fallback: scan `/sys/class/tty/ttyUSB*/device/../` for Elad VID:PID
/// 3. Fallback: scan `/sys/class/tty/ttyACM*/device/../` for Elad VID:PID
pub fn discover_serial_device() -> Result<Option<String>, CatError> {
    // Strategy 1: /dev/serial/by-id/
    if let Some(dev) = discover_by_id()? {
        return Ok(Some(dev));
    }

    // Strategy 2: match USB VID:PID via sysfs
    if let Some(dev) = discover_by_vid_pid()? {
        return Ok(Some(dev));
    }

    debug!("no FDM-DUO serial device found");
    Ok(None)
}

/// Scan `/dev/serial/by-id/` for a symlink matching the Elad FDM-DUO.
fn discover_by_id() -> Result<Option<String>, CatError> {
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
                info!(symlink = %name_str, device = %device, "found FDM-DUO serial device (by-id)");
                return Ok(Some(device));
            }
        }
    }

    Ok(None)
}

/// Scan sysfs for ttyUSB* / ttyACM* devices whose parent USB device
/// matches the Elad VID:PID (1721:061a).
fn discover_by_vid_pid() -> Result<Option<String>, CatError> {
    for prefix in &["ttyUSB", "ttyACM"] {
        let class_dir = Path::new("/sys/class/tty");
        if !class_dir.exists() {
            continue;
        }

        let entries = match std::fs::read_dir(class_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with(prefix) {
                continue;
            }

            // Read the USB device's idVendor and idProduct from sysfs
            // Path: /sys/class/tty/ttyUSBx/device/../idVendor
            let device_dir = entry.path().join("device").join("..");
            let vid = read_sysfs_attr(&device_dir.join("idVendor"));
            let pid = read_sysfs_attr(&device_dir.join("idProduct"));

            if let (Some(vid), Some(pid)) = (vid, pid) {
                if vid == ELAD_VID && pid == ELAD_PID {
                    let dev_path = format!("/dev/{}", name_str);
                    if Path::new(&dev_path).exists() {
                        info!(device = %dev_path, "found FDM-DUO serial device (sysfs VID:PID)");
                        return Ok(Some(dev_path));
                    }
                }
            }
        }
    }

    Ok(None)
}

/// Read a sysfs attribute file, returning trimmed contents.
fn read_sysfs_attr(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
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
