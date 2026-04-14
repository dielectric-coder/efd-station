use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub usb: UsbConfig,
    pub dsp: DspConfig,
    pub cat: CatConfig,
    pub audio: AudioConfig,
    pub drm: DrmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsbConfig {
    pub vendor_id: u16,
    pub product_id: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DspConfig {
    pub fft_size: usize,
    pub fft_averaging: usize,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatConfig {
    /// Serial device for CAT control. "auto" discovers the FDM-DUO CAT port.
    /// Or an explicit path like "/dev/ttyUSB0".
    pub serial_device: String,
    pub poll_interval_ms: u64,
    /// rigctld-compatible TCP responder fronting the FDM-DUO (native CAT).
    /// Bound only when the active source has hardware CAT.
    pub responder_fdmduo_bind: String,
    /// rigctld-compatible TCP responder fronting the software demod.
    /// Bound only when the active source can supply IQ.
    pub responder_demod_bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DrmConfig {
    /// Path to the dream binary. Defaults to "dream" on PATH; set to the
    /// vendored build path (e.g. "/usr/lib/efd-station/dream") when packaged.
    pub dream_binary: String,
    /// Initial state of dream's `-p` flag. Per-runtime override comes
    /// from the client via `ClientMsg::SetDrmFlipSpectrum`. Some DRM
    /// broadcasters transmit with inverted spectrum (one of DREAM's
    /// bundled samples is labeled `..._flipped_spectrum.flac`); DREAM
    /// has no auto-detection.
    pub flip_spectrum: bool,
    /// Deprecated (removed in 0.7.0): was the PipeWire null-sink name
    /// for audio-IF into DREAM. snd-aloop replaced null sinks, so the
    /// field is accepted-but-ignored for config back-compat. Remove
    /// from your config.toml at your leisure.
    #[serde(skip_serializing, default)]
    #[allow(dead_code)]
    pub input_sink: Option<String>,
    /// Deprecated (removed in 0.7.0): was the PipeWire null-sink name
    /// for dream's decoded audio. See `input_sink` above.
    #[serde(skip_serializing, default)]
    #[allow(dead_code)]
    pub output_sink: Option<String>,
}

impl Default for DrmConfig {
    fn default() -> Self {
        Self {
            dream_binary: "dream".into(),
            flip_spectrum: false,
            input_sink: None,
            output_sink: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// ALSA device for RX audio playback (HAT sound card).
    pub alsa_device: String,
    /// ALSA device for TX audio output to FDM-DUO USB audio.
    pub tx_device: String,
    /// ALSA device for RX audio capture from FDM-DUO USB audio.
    pub rx_device: String,
    pub sample_rate: u32,
}

// -- defaults --


impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0".into(),
            port: 8080,
        }
    }
}

impl Default for UsbConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0x1721,
            product_id: 0x061a,
        }
    }
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            fft_size: 4096,
            fft_averaging: 3,
            sample_rate: 192_000,
        }
    }
}

impl Default for CatConfig {
    fn default() -> Self {
        Self {
            serial_device: "auto".into(),
            poll_interval_ms: 200,
            responder_fdmduo_bind: "127.0.0.1:4532".into(),
            responder_demod_bind: "127.0.0.1:4533".into(),
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            alsa_device: "default".into(),
            tx_device: "auto".into(),
            rx_device: "auto".into(),
            sample_rate: 48_000,
        }
    }
}

/// Resolve the config file path: `~/.config/efd-backend/config.toml`
pub fn config_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "efd-backend")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

/// Load config from disk, falling back to defaults for missing fields.
///
/// All sub-structs use `#[serde(default, deny_unknown_fields)]`, so any
/// typo, unknown field, or type mismatch in the user's `config.toml`
/// produces a loud parse error rather than silently using defaults.
/// The server still starts (degrades to full defaults) so a misconfig
/// doesn't prevent operator recovery, but the warning log will tell
/// them exactly what went wrong.
pub fn load() -> Config {
    let path = config_path();
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => {
                tracing::info!(path = %path.display(), "config loaded");
                cfg
            }
            Err(e) => {
                // The error includes line/column when toml's deserializer
                // knows where the problem is (unknown field, type
                // mismatch). Make it impossible to miss.
                tracing::warn!(
                    "\n\
                    ============================================================\n\
                    config parse error — FALLING BACK TO DEFAULTS\n\
                    file:  {}\n\
                    error: {e}\n\
                    Common causes:\n\
                      - typo in a field name (e.g. `flipspectrum` not `flip_spectrum`)\n\
                      - wrong type (e.g. `flip_spectrum = \"true\"` instead of `flip_spectrum = true`)\n\
                      - field placed outside its `[section]` (e.g. under `[server]` instead of `[drm]`)\n\
                    ============================================================\n",
                    path.display()
                );
                Config::default()
            }
        },
        Err(_) => {
            tracing::info!(path = %path.display(), "no config file, using defaults");
            Config::default()
        }
    };
    log_effective(&cfg);
    cfg
}

/// Log the effective config so operators can tell at a glance what
/// actually took effect — the best defense against "I set X but it
/// seems to be ignored" confusion.
fn log_effective(cfg: &Config) {
    tracing::info!(
        bind = %cfg.server.bind,
        port = cfg.server.port,
        "effective server config"
    );
    tracing::info!(
        serial = %cfg.cat.serial_device,
        poll_ms = cfg.cat.poll_interval_ms,
        fdmduo_bind = %cfg.cat.responder_fdmduo_bind,
        demod_bind = %cfg.cat.responder_demod_bind,
        "effective cat config"
    );
    tracing::info!(
        alsa = %cfg.audio.alsa_device,
        tx = %cfg.audio.tx_device,
        rx = %cfg.audio.rx_device,
        rate = cfg.audio.sample_rate,
        "effective audio config"
    );
    tracing::info!(
        dream = %cfg.drm.dream_binary,
        flip_spectrum = cfg.drm.flip_spectrum,
        "effective drm config"
    );
    if cfg.drm.input_sink.is_some() || cfg.drm.output_sink.is_some() {
        tracing::warn!(
            "[drm] input_sink / output_sink are deprecated since 0.7.0 \
             (snd-aloop replaced PipeWire null sinks) and are ignored. \
             Remove them from your config.toml to silence this."
        );
    }
    tracing::debug!(?cfg, "effective config (full)");
}
