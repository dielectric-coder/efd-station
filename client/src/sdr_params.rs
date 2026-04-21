use efd_proto::{AgcMode, Mode};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persisted SDR operating parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdrParams {
    pub freq_hz: u64,
    pub mode: String,
    pub agc_threshold: u8,
    /// AGC speed: "slow" / "medium" / "fast". Serialized as a string
    /// so the TOML file is easy to hand-edit; maps to [`AgcMode`] via
    /// [`Self::agc_mode`] / [`Self::set_agc_mode`]. Older files with
    /// no `agc_speed` key load with the serde default ("slow").
    #[serde(default = "default_agc_speed")]
    pub agc_speed: String,
}

fn default_agc_speed() -> String {
    "slow".into()
}

impl Default for SdrParams {
    fn default() -> Self {
        Self {
            freq_hz: 7_100_000,
            mode: "USB".into(),
            agc_threshold: 5,
            agc_speed: default_agc_speed(),
        }
    }
}

impl SdrParams {
    pub fn mode(&self) -> Mode {
        match self.mode.as_str() {
            "LSB" => Mode::LSB,
            "USB" => Mode::USB,
            "CW" => Mode::CW,
            "CWR" => Mode::CWR,
            "AM" => Mode::AM,
            "FM" => Mode::FM,
            "DRM" => Mode::DRM,
            _ => Mode::USB,
        }
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode_str(mode).to_string();
    }

    pub fn agc_mode(&self) -> AgcMode {
        match self.agc_speed.as_str() {
            "off" => AgcMode::Off,
            "fast" => AgcMode::Fast,
            "medium" => AgcMode::Medium,
            _ => AgcMode::Slow,
        }
    }

    pub fn set_agc_mode(&mut self, mode: AgcMode) {
        self.agc_speed = match mode {
            AgcMode::Off => "off",
            AgcMode::Fast => "fast",
            AgcMode::Medium => "medium",
            AgcMode::Slow => "slow",
        }
        .to_string();
    }
}

pub fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::LSB => "LSB",
        Mode::USB => "USB",
        Mode::CW => "CW",
        Mode::CWR => "CWR",
        Mode::AM => "AM",
        Mode::SAM => "SAM",
        Mode::SAMU => "SAM-U",
        Mode::SAML => "SAM-L",
        Mode::DSB => "DSB",
        Mode::FM => "FM",
        Mode::DRM => "DRM",
        Mode::Unknown => "USB",
    }
}

fn params_path() -> PathBuf {
    directories::ProjectDirs::from("", "", "efd-client")
        .map(|d| d.config_dir().join("sdr_params.toml"))
        .unwrap_or_else(|| PathBuf::from("sdr_params.toml"))
}

pub fn load() -> SdrParams {
    let path = params_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<SdrParams>(&text) {
            Ok(p) => {
                tracing::info!(path = %path.display(), "SDR params loaded");
                p
            }
            Err(e) => {
                tracing::warn!(err = %e, "bad SDR params file, using defaults");
                SdrParams::default()
            }
        },
        Err(_) => {
            tracing::info!("no SDR params file, using defaults");
            SdrParams::default()
        }
    }
}

pub fn save(params: &SdrParams) {
    let path = params_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match toml::to_string_pretty(params) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                tracing::warn!(err = %e, "failed to save SDR params");
            } else {
                tracing::info!(path = %path.display(), "SDR params saved");
            }
        }
        Err(e) => tracing::warn!(err = %e, "failed to serialize SDR params"),
    }
}
