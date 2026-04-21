//! Device discovery.
//!
//! Enumerates the devices the backend could plausibly drive:
//!
//! - **IQ-USB devices** (FDM-DUO, HackRF, RSPdx, RTL-SDR) via sysfs
//!   VID/PID walk. Same strategy the per-crate discoveries already
//!   use, just generalised to a whole table. Only FDM-DUO has a
//!   driver wired up in `efd-iq` today; the others are listed so the
//!   client surfaces them honestly as "plugged in but not yet
//!   supported" rather than silently hiding them.
//!
//! - **Audio-in devices** via the existing
//!   [`efd_audio::discover_alsa_devices`] plus a secondary sweep of
//!   `/proc/asound/cards` so standalone USB audio dongles
//!   (portable-radio configs) also show up.
//!
//! - **CAT serial devices** via the existing
//!   [`efd_cat::discover_serial_device`].
//!
//! Results are returned as `efd_proto::DeviceList` so the WS layer
//! can forward them to clients unchanged.
//!
//! Discovery is a read-only sysfs / filesystem walk — nothing is
//! opened or claimed. That's deliberate: we want to be able to enumerate
//! even when another process currently holds a device.

use std::path::Path;

use efd_proto::{DeviceId, DeviceList, SourceKind};
use tracing::debug;

/// Known VID/PID pairs. Missing an SDR? Add here; discovery picks it up
/// automatically.
const KNOWN_IQ_DEVICES: &[(SourceKind, &str, &str, &str)] = &[
    // (kind,           vid,    pid,    human-readable label)
    (SourceKind::FdmDuo, "1721", "061a", "Elad FDM-DUO"),
    // HackRF One (Great Scott Gadgets).
    (SourceKind::HackRf, "1d50", "6089", "HackRF One"),
    // RTL-SDR (Realtek RTL2832U) — the dominant dongle variant.
    (SourceKind::RtlSdr, "0bda", "2838", "RTL-SDR (RTL2832U)"),
    // SDRplay RSPdx (SDRplay Ltd).
    (SourceKind::RspDx, "1df7", "3000", "SDRplay RSPdx"),
];

/// Run the full discovery sweep.
///
/// The `active` field is left as `None`; callers that know what's
/// actually running (server main after startup) overwrite it.
pub fn enumerate() -> DeviceList {
    let mut iq_devices = enumerate_iq_usb();
    let mut audio_devices = enumerate_audio_in();

    // Always offer file-replay as a synthetic "device". The concrete
    // file path will come in via `SelectDevice(DeviceId { kind:
    // AudioFile, id: path })` from the client in phase 4; for now
    // the placeholder keeps the UI consistent.
    audio_devices.push(DeviceId {
        kind: SourceKind::AudioFile,
        id: String::new(),
    });
    iq_devices.push(DeviceId {
        kind: SourceKind::IqFile,
        id: String::new(),
    });

    DeviceList {
        audio_devices,
        iq_devices,
        active: None,
    }
}

/// Walk `/sys/bus/usb/devices` for every known IQ-USB VID/PID.
fn enumerate_iq_usb() -> Vec<DeviceId> {
    let mut out: Vec<DeviceId> = Vec::new();
    let usb_devices = Path::new("/sys/bus/usb/devices");
    let Ok(entries) = std::fs::read_dir(usb_devices) else {
        debug!("no /sys/bus/usb/devices — skipping IQ discovery");
        return out;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip `N-M:a.b` interface entries — only match the device-level
        // entries that carry idVendor / idProduct.
        if name.contains(':') {
            continue;
        }
        let dev_path = entry.path();
        let vid = read_sysfs(&dev_path.join("idVendor"));
        let pid = read_sysfs(&dev_path.join("idProduct"));
        let (Some(v), Some(p)) = (vid, pid) else { continue };

        for (kind, want_v, want_p, label) in KNOWN_IQ_DEVICES {
            if v == *want_v && p == *want_p {
                let id = usb_id(&dev_path, &name);
                debug!(kind = ?kind, %id, %label, "discovered IQ USB device");
                out.push(DeviceId { kind: *kind, id });
            }
        }
    }

    out
}

/// Enumerate ALSA capture devices as generic audio inputs. Every
/// capture-capable card in `/proc/asound/cards` becomes a
/// `SourceKind::PortableRadio` entry with id `"hw:N,0"`, regardless
/// of what hardware backs it. The FDM-DUO's USB audio port shows up
/// here too — logically it's just another USB audio card, distinct
/// from the IQ side which is enumerated under `SourceKind::FdmDuo`
/// in `enumerate_iq_usb`. Keeping the two sides of the radio as
/// separate `DeviceId`s removes the ambiguity that the single
/// `SourceKind::FdmDuo` used to carry in both lists.
fn enumerate_audio_in() -> Vec<DeviceId> {
    let mut out: Vec<DeviceId> = Vec::new();

    if let Ok(cards) = std::fs::read_to_string("/proc/asound/cards") {
        for line in cards.lines() {
            // Lines look like ` 0 [Loopback       ]: Loopback - Loopback ...`
            // We parse the leading card index.
            let trimmed = line.trim_start();
            let Some(idx_end) = trimmed.find(' ') else { continue };
            let Ok(card_idx) = trimmed[..idx_end].parse::<u32>() else {
                continue;
            };
            out.push(DeviceId {
                kind: SourceKind::PortableRadio,
                id: format!("hw:{card_idx},0"),
            });
        }
    } else {
        debug!("no /proc/asound/cards — audio-in enumeration skipped");
    }

    out
}

/// Best-effort USB identifier for a device. Prefers the manufacturer
/// "serial" string; falls back to the sysfs port name (`1-2.3`) if the
/// serial isn't readable. Serial survives replug; port name doesn't,
/// but it's stable enough for a single session.
fn usb_id(dev_path: &Path, sysfs_name: &str) -> String {
    read_sysfs(&dev_path.join("serial")).unwrap_or_else(|| sysfs_name.to_string())
}

fn read_sysfs(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}
