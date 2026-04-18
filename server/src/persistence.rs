//! Session state persistence.
//!
//! On shutdown the backend writes its current
//! [`efd_proto::StateSnapshot`] as TOML to
//! `$XDG_STATE_HOME/efd-backend/state.toml` (default
//! `~/.local/state/efd-backend/state.toml`). On startup it reads the
//! file back, validates that the referenced device is still present
//! among the discovered ones, and hands the result to the pipeline so
//! the user lands where they were last time without reconfiguring.
//!
//! TOML was chosen to match the existing config.toml. The schema is
//! the proto type itself (serde-derived), so adding a field to
//! `StateSnapshot` in `efd-proto` automatically extends the persisted
//! format without a migration — unknown-on-read fields fall back to
//! the `Default` value.

use std::path::{Path, PathBuf};

use efd_proto::{DeviceList, Mode, StateSnapshot};
use tracing::{debug, info, warn};

/// Resolve the on-disk path: `$XDG_STATE_HOME/efd-backend/state.toml`,
/// falling back to `state.toml` in the working directory if the XDG
/// lookup fails (no home, no tmpdir). Matches the config-path fallback
/// shape in [`crate::config::config_path`].
pub fn state_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "efd-backend")
        .map(|d| d.state_dir().map(PathBuf::from).unwrap_or_else(|| d.config_dir().join("state")))
        .map(|dir| dir.join("state.toml"))
        .unwrap_or_else(|| PathBuf::from("state.toml"))
}

/// Provide a zero-state default when nothing has been saved yet. The
/// pipeline uses this as the floor before device discovery decides on
/// an initial active device.
pub fn default_snapshot() -> StateSnapshot {
    StateSnapshot {
        active_device: None,
        freq_hz: 14_074_000, // 20 m FT8 is as defensible a default as any.
        mode: Mode::USB,
        filter_bw_hz: Some(2_400.0),
        rit_hz: 0,
        xit_hz: 0,
        if_offset_hz: 0,
        enabled_decoders: Vec::new(),
        nb_on: false,
        dnb_on: false,
        dnr_on: false,
        dnf_on: false,
        apf_on: false,
    }
}

/// Try to read the last-saved state. Returns `None` on first run or if
/// the file is malformed — the pipeline falls back to defaults in that
/// case, same as it would without persistence at all.
pub fn load() -> Option<StateSnapshot> {
    let path = state_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no saved state yet");
            return None;
        }
        Err(e) => {
            warn!(path = %path.display(), "cannot read state file: {e}");
            return None;
        }
    };
    match toml::from_str::<StateSnapshot>(&text) {
        Ok(s) => {
            info!(path = %path.display(), "state loaded");
            Some(s)
        }
        Err(e) => {
            warn!(
                path = %path.display(),
                "state parse error (ignoring): {e}"
            );
            None
        }
    }
}

/// Validate a loaded snapshot against the devices the server can
/// actually see. If the saved device is gone (unplugged, replaced),
/// clear `active_device` so the pipeline picks a fresh one.
pub fn validate(snap: &mut StateSnapshot, discovered: &DeviceList) {
    let Some(wanted) = &snap.active_device else {
        return;
    };
    let present = discovered
        .audio_devices
        .iter()
        .chain(discovered.iq_devices.iter())
        .any(|d| d == wanted);
    if !present {
        warn!(
            ?wanted,
            "saved active_device not present after discovery — clearing"
        );
        snap.active_device = None;
    }
}

/// Persist the snapshot. Best-effort: if the directory can't be
/// created or the write fails, we log and move on — losing the
/// snapshot is annoying but not fatal.
pub fn save(snap: &StateSnapshot) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(dir = %parent.display(), "cannot create state dir: {e}");
            return;
        }
    }
    let text = match toml::to_string_pretty(snap) {
        Ok(t) => t,
        Err(e) => {
            warn!("state serialise failed: {e}");
            return;
        }
    };
    if let Err(e) = atomic_write(&path, &text) {
        warn!(path = %path.display(), "state write failed: {e}");
    } else {
        info!(path = %path.display(), "state saved");
    }
}

/// Write through a sibling `.tmp` file and rename — same technique
/// config files across Linux use, so a crash mid-write doesn't leave
/// half a file behind.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let mut tmp = path.to_path_buf();
    tmp.set_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
