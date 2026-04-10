use std::path::Path;

use tracing::{debug, info};

use crate::error::CatError;

const ELAD_SERIAL_PATTERNS: &[&str] = &["ELAD", "FDM-DUO", "FDM_DUO"];
const ELAD_VID: &str = "1721";
const ELAD_PID: &str = "061a";

/// Auto-discover the FDM-DUO CAT serial port.
///
/// The FDM-DUO has 3 USB interfaces (IQ, audio, CAT serial) that appear
/// as separate USB devices on the same hub. The CAT port is an FTDI FT232
/// (0403:6001) that shows up as /dev/ttyUSBx.
///
/// Discovery strategies (in order):
/// 1. Check for udev symlink `/dev/fdm-duo-cat`
/// 2. Scan `/dev/serial/by-id/` for ELAD/FDM-DUO names
/// 3. Find Elad IQ device (1721:061a) in sysfs, then find ttyUSB*/ttyACM*
///    sibling on the same USB hub
pub fn discover_serial_device() -> Result<Option<String>, CatError> {
    // Strategy 0: udev symlink
    let udev_path = Path::new("/dev/fdm-duo-cat");
    if udev_path.exists() {
        let resolved = udev_path.canonicalize().map_err(CatError::Io)?;
        let device = resolved.to_string_lossy().to_string();
        info!(device = %device, "found FDM-DUO CAT serial (udev symlink)");
        return Ok(Some(device));
    }

    // Strategy 1: /dev/serial/by-id/
    if let Some(dev) = discover_by_id()? {
        return Ok(Some(dev));
    }

    // Strategy 2: sysfs hub sibling
    if let Some(dev) = discover_by_hub_sibling()? {
        return Ok(Some(dev));
    }

    debug!("no FDM-DUO serial device found");
    Ok(None)
}

fn discover_by_id() -> Result<Option<String>, CatError> {
    let by_id = Path::new("/dev/serial/by-id");
    if !by_id.exists() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(by_id).map_err(CatError::Io)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        for pattern in ELAD_SERIAL_PATTERNS {
            if name_str.contains(pattern) {
                let resolved = entry.path().canonicalize().map_err(CatError::Io)?;
                let device = resolved.to_string_lossy().to_string();
                info!(device = %device, "found FDM-DUO CAT serial (by-id)");
                return Ok(Some(device));
            }
        }
    }
    Ok(None)
}

fn discover_by_hub_sibling() -> Result<Option<String>, CatError> {
    let usb_devices = Path::new("/sys/bus/usb/devices");
    if !usb_devices.exists() {
        return Ok(None);
    }

    // Find Elad IQ device and its hub prefix
    let hub_prefix = match find_elad_hub_prefix(usb_devices)? {
        Some(p) => p,
        None => return Ok(None),
    };

    debug!(hub_prefix = %hub_prefix, "found Elad device, scanning siblings");

    // Scan siblings for a tty device
    for entry in std::fs::read_dir(usb_devices).map_err(CatError::Io)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&hub_prefix) || name.contains(':') {
            continue;
        }
        if let Some(tty) = find_tty_under(&entry.path()) {
            let dev_path = format!("/dev/{tty}");
            if Path::new(&dev_path).exists() {
                info!(device = %dev_path, usb_port = %name, "found FDM-DUO CAT serial (hub sibling)");
                return Ok(Some(dev_path));
            }
        }
    }
    Ok(None)
}

fn find_elad_hub_prefix(usb_devices: &Path) -> Result<Option<String>, CatError> {
    for entry in std::fs::read_dir(usb_devices).map_err(CatError::Io)?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(':') {
            continue;
        }
        let vid = read_sysfs(&entry.path().join("idVendor"));
        let pid = read_sysfs(&entry.path().join("idProduct"));
        if let (Some(v), Some(p)) = (vid, pid) {
            if v == ELAD_VID && p == ELAD_PID {
                if let Some(dot) = name.rfind('.') {
                    return Ok(Some(name[..=dot].to_string()));
                }
            }
        }
    }
    Ok(None)
}

fn find_tty_under(path: &Path) -> Option<String> {
    walk(path, 4)
}

fn walk(path: &Path, depth: usize) -> Option<String> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ttyUSB") || name.starts_with("ttyACM") {
            return Some(name);
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(found) = walk(&entry.path(), depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn read_sysfs(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}
