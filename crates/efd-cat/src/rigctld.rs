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
/// The FDM-DUO exposes 3 USB interfaces through an internal hub:
///   - IQ data (VID 1721:061a)
///   - Audio (USB audio class, e.g. C-Media 0d8c:01af)
///   - CAT serial (FTDI 0403:6001 → /dev/ttyUSBx)
///
/// Strategy:
/// 1. Scan `/dev/serial/by-id/` for symlinks matching ELAD/FDM-DUO
/// 2. Fallback: find the Elad IQ device (1721:061a) in sysfs, then find
///    a ttyUSB*/ttyACM* sibling on the same USB hub
pub fn discover_serial_device() -> Result<Option<String>, CatError> {
    // Strategy 1: /dev/serial/by-id/
    if let Some(dev) = discover_by_id()? {
        return Ok(Some(dev));
    }

    // Strategy 2: find serial port sibling of Elad IQ device on same hub
    if let Some(dev) = discover_by_hub_sibling()? {
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

/// Find the Elad IQ device (1721:061a) in `/sys/bus/usb/devices/`, then
/// look for a ttyUSB*/ttyACM* serial port on a sibling port of the same hub.
///
/// Example sysfs layout (CM5):
///   4-1.1  → 1721:061a  (IQ)
///   4-1.2  → 0d8c:01af  (audio)
///   4-1.3  → 0403:6001  (FTDI serial → /dev/ttyUSB0)
///
/// We find 4-1.1 as the Elad device, extract hub prefix "4-1.",
/// then check all "4-1.*" siblings for a ttyUSB*/ttyACM* child.
fn discover_by_hub_sibling() -> Result<Option<String>, CatError> {
    let usb_devices = Path::new("/sys/bus/usb/devices");
    if !usb_devices.exists() {
        return Ok(None);
    }

    // Step 1: find the Elad IQ device and its hub prefix
    let hub_prefix = find_elad_hub_prefix(usb_devices)?;
    let hub_prefix = match hub_prefix {
        Some(p) => p,
        None => {
            debug!("Elad IQ device (1721:061a) not found in sysfs");
            return Ok(None);
        }
    };

    debug!(hub_prefix = %hub_prefix, "found Elad device, scanning siblings");

    // Step 2: scan sibling devices on the same hub for a tty
    let entries = std::fs::read_dir(usb_devices).map_err(CatError::Io)?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();

        // Must be a sibling port (same hub prefix, no colon = device not interface)
        if !name_str.starts_with(&hub_prefix) || name_str.contains(':') {
            continue;
        }

        // Look for a ttyUSB* or ttyACM* anywhere under this device
        if let Some(tty) = find_tty_under(&entry.path()) {
            let dev_path = format!("/dev/{tty}");
            if Path::new(&dev_path).exists() {
                info!(
                    device = %dev_path,
                    usb_port = %name_str,
                    "found FDM-DUO CAT serial (hub sibling)"
                );
                return Ok(Some(dev_path));
            }
        }
    }

    Ok(None)
}

/// Find the Elad IQ device (1721:061a) and return its hub prefix.
/// E.g. if device is "4-1.1", returns "4-1." so siblings are "4-1.*".
fn find_elad_hub_prefix(usb_devices: &Path) -> Result<Option<String>, CatError> {
    let entries = std::fs::read_dir(usb_devices).map_err(CatError::Io)?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();

        // Skip interface entries (contain ':')
        if name_str.contains(':') {
            continue;
        }

        let vid = read_sysfs_attr(&entry.path().join("idVendor"));
        let pid = read_sysfs_attr(&entry.path().join("idProduct"));

        if let (Some(vid), Some(pid)) = (vid, pid) {
            if vid == ELAD_VID && pid == ELAD_PID {
                // Extract hub prefix: "4-1.1" → "4-1."
                if let Some(last_dot) = name_str.rfind('.') {
                    let prefix = &name_str[..=last_dot]; // includes the dot
                    return Ok(Some(prefix.to_string()));
                }
            }
        }
    }

    Ok(None)
}

/// Recursively search under a sysfs USB device path for a ttyUSB* or ttyACM* entry.
fn find_tty_under(path: &Path) -> Option<String> {
    // Check direct children and nested paths for tty directory entries
    let walker = walkdir(path, 4); // max 4 levels deep
    for name in walker {
        if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
            return Some(name);
        }
    }
    None
}

/// Simple recursive directory name collector (avoids adding walkdir dependency).
fn walkdir(path: &Path, max_depth: usize) -> Vec<String> {
    let mut results = Vec::new();
    walkdir_inner(path, max_depth, &mut results);
    results
}

fn walkdir_inner(path: &Path, depth: usize, results: &mut Vec<String>) {
    if depth == 0 {
        return;
    }
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
            results.push(name);
        }
        let ft = entry.file_type();
        if ft.map(|t| t.is_dir()).unwrap_or(false) {
            walkdir_inner(&entry.path(), depth - 1, results);
        }
    }
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
