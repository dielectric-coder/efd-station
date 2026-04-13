use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub usb: UsbConfig,
    pub dsp: DspConfig,
    pub cat: CatConfig,
    pub audio: AudioConfig,
    pub drm: DrmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UsbConfig {
    pub vendor_id: u16,
    pub product_id: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DspConfig {
    pub fft_size: usize,
    pub fft_averaging: usize,
    pub sample_rate: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
#[serde(default)]
pub struct DrmConfig {
    /// Path to the dream binary. Defaults to "dream" on PATH; set to the
    /// vendored build path (e.g. "/usr/lib/efd-station/dream") when packaged.
    pub dream_binary: String,
    /// PipeWire null-sink name for IQ fed to dream.
    pub input_sink: String,
    /// PipeWire null-sink name for dream's decoded audio output.
    pub output_sink: String,
}

impl Default for DrmConfig {
    fn default() -> Self {
        Self {
            dream_binary: "dream".into(),
            input_sink: "efd_drm_in".into(),
            output_sink: "efd_drm_out".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => {
                tracing::info!(path = %path.display(), "config loaded");
                cfg
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "bad config, using defaults");
                Config::default()
            }
        },
        Err(_) => {
            tracing::info!(path = %path.display(), "no config file, using defaults");
            Config::default()
        }
    }
}
