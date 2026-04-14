use std::path::Path;

use tracing::{debug, info, warn};

const ELAD_VID: &str = "1721";
const ELAD_PID: &str = "061a";

/// Discovered ALSA devices for the FDM-DUO USB audio interface.
#[derive(Debug, Clone)]
pub struct FdmDuoAlsa {
    /// ALSA capture device for RX audio (hardware demod output), e.g. "hw:2,0".
    pub capture: Option<String>,
    /// ALSA playback device for TX audio input to radio, e.g. "hw:2,0".
    pub playback: Option<String>,
}

/// Auto-discover the FDM-DUO USB audio ALSA devices.
///
/// The FDM-DUO presents multiple USB interfaces on the same hub.  We locate
/// the IQ interface (1721:061a) in sysfs, then scan sibling devices for an
/// ALSA sound card.  Once the card number is known we check which PCM capture
/// and playback sub-devices are available.
pub fn discover_alsa_devices() -> Option<FdmDuoAlsa> {
    let usb_devices = Path::new("/sys/bus/usb/devices");
    if !usb_devices.exists() {
        return None;
    }

    let hub_prefix = find_elad_hub_prefix(usb_devices)?;
    debug!(hub_prefix = %hub_prefix, "found Elad device, scanning siblings for sound card");

    // Scan hub siblings for an ALSA sound card.
    for entry in std::fs::read_dir(usb_devices).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&hub_prefix) || name.contains(':') {
            continue;
        }

        if let Some(card_num) = find_sound_card_under(&entry.path()) {
            info!(card = card_num, usb_port = %name, "found FDM-DUO audio card");
            let devices = resolve_pcm_devices(card_num);
            return Some(devices);
        }
    }

    // Also check if the Elad IQ device itself has an audio interface
    // (composite USB device with multiple interfaces).
    for entry in std::fs::read_dir(usb_devices).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(':') {
            continue;
        }
        let vid = read_sysfs(&entry.path().join("idVendor"));
        let pid = read_sysfs(&entry.path().join("idProduct"));
        if let (Some(v), Some(p)) = (vid, pid) {
            if v == ELAD_VID && p == ELAD_PID {
                if let Some(card_num) = find_sound_card_under(&entry.path()) {
                    info!(card = card_num, "found FDM-DUO audio card (same device)");
                    let devices = resolve_pcm_devices(card_num);
                    return Some(devices);
                }
            }
        }
    }

    debug!("no FDM-DUO ALSA sound card found");
    None
}

/// Probe whether an ALSA capture device can actually be opened.
/// Used as a runtime gate for `Capabilities::has_usb_audio` —
/// resolving the device *name* from sysfs isn't enough; the open
/// itself can fail with ENOTSUPP (errno 524) if another process
/// (PipeWire/PulseAudio) has claimed it or the format isn't
/// supported. The returned PCM is dropped immediately, releasing
/// the device for the real capture task to reopen.
pub fn probe_capture(device: &str) -> bool {
    use alsa::{Direction, PCM};
    match PCM::new(device, Direction::Capture, false) {
        Ok(_pcm) => true,
        Err(e) => {
            warn!(device, error = %e, "USB RX audio probe failed");
            false
        }
    }
}

/// Resolve a single ALSA device name for `rx_device` or `tx_device`.
///
/// If `configured` is `"auto"`, runs discovery and returns the appropriate
/// capture or playback device.  Otherwise returns the configured value as-is.
/// Returns `None` only when auto-discovery finds nothing.
pub fn resolve_device(configured: &str, capture: bool) -> Option<String> {
    if configured != "auto" {
        if configured.is_empty() {
            return None;
        }
        return Some(configured.to_string());
    }
    let devs = discover_alsa_devices()?;
    let dev = if capture { devs.capture } else { devs.playback };
    if dev.is_none() {
        warn!(
            "auto-discovery found FDM-DUO but no {} PCM device",
            if capture { "capture" } else { "playback" }
        );
    }
    dev
}

// --- internals ---

fn find_elad_hub_prefix(usb_devices: &Path) -> Option<String> {
    for entry in std::fs::read_dir(usb_devices).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.contains(':') {
            continue;
        }
        let vid = read_sysfs(&entry.path().join("idVendor"));
        let pid = read_sysfs(&entry.path().join("idProduct"));
        if let (Some(v), Some(p)) = (vid, pid) {
            if v == ELAD_VID && p == ELAD_PID {
                // Hub prefix: "1-2." from "1-2.3"
                if let Some(dot) = name.rfind('.') {
                    return Some(name[..=dot].to_string());
                }
            }
        }
    }
    None
}

/// Walk a USB device path looking for a `sound/cardN` directory.
/// Returns the card number if found.
fn find_sound_card_under(path: &Path) -> Option<u32> {
    walk_for_card(path, 5)
}

fn walk_for_card(path: &Path, depth: usize) -> Option<u32> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(num_str) = name.strip_prefix("card") {
            if let Ok(num) = num_str.parse::<u32>() {
                // Confirm it's actually under the sound subsystem
                let parent = entry.path();
                let parent_name = parent
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if parent_name == "sound" {
                    return Some(num);
                }
            }
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(found) = walk_for_card(&entry.path(), depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// Given a card number, check /sys/class/sound for pcmC{card}D{dev}c (capture)
/// and pcmC{card}D{dev}p (playback) entries and build ALSA device names.
fn resolve_pcm_devices(card: u32) -> FdmDuoAlsa {
    let sound_class = Path::new("/sys/class/sound");
    let mut capture = None;
    let mut playback = None;

    let prefix = format!("pcmC{card}D");
    if let Ok(entries) = std::fs::read_dir(sound_class) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(&prefix) {
                continue;
            }
            // Parse device number from pcmC{card}D{dev}{c|p}
            let suffix = &name[prefix.len()..];
            let is_capture = suffix.ends_with('c');
            let is_playback = suffix.ends_with('p');
            let dev_str = &suffix[..suffix.len() - 1];
            if let Ok(dev) = dev_str.parse::<u32>() {
                let hw = format!("hw:{card},{dev}");
                if is_capture && capture.is_none() {
                    debug!(device = %hw, "found FDM-DUO capture PCM");
                    capture = Some(hw);
                } else if is_playback && playback.is_none() {
                    debug!(device = %hw, "found FDM-DUO playback PCM");
                    playback = Some(hw);
                }
            }
        }
    }

    FdmDuoAlsa { capture, playback }
}

fn read_sysfs(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}
