//! Source-backend abstraction.
//!
//! `SourceConfig` picks one of the supported RF sources and carries its
//! per-backend configuration. Its `capabilities()` method drives the server
//! `Capabilities` message and the client UI gating. Only `FdmDuo` has a
//! working capture implementation today; other variants are placeholders so
//! the pipeline wiring is ready when each backend lands.

use efd_proto::{Mode, SourceKind};

use crate::device::{ELAD_PRODUCT_ID, ELAD_VENDOR_ID};

#[derive(Debug, Clone)]
pub struct FdmDuoConfig {
    pub vendor_id: u16,
    pub product_id: u16,
}

impl Default for FdmDuoConfig {
    fn default() -> Self {
        Self {
            vendor_id: ELAD_VENDOR_ID,
            product_id: ELAD_PRODUCT_ID,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HackRfConfig {
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RspDxConfig {
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RtlSdrConfig {
    pub index: u32,
}

#[derive(Debug, Clone, Default)]
pub struct PortableRadioConfig;

#[derive(Debug, Clone)]
pub enum SourceConfig {
    FdmDuo(FdmDuoConfig),
    HackRf(HackRfConfig),
    RspDx(RspDxConfig),
    RtlSdr(RtlSdrConfig),
    PortableRadio(PortableRadioConfig),
}

impl SourceConfig {
    pub fn kind(&self) -> SourceKind {
        match self {
            SourceConfig::FdmDuo(_) => SourceKind::FdmDuo,
            SourceConfig::HackRf(_) => SourceKind::HackRf,
            SourceConfig::RspDx(_) => SourceKind::RspDx,
            SourceConfig::RtlSdr(_) => SourceKind::RtlSdr,
            SourceConfig::PortableRadio(_) => SourceKind::PortableRadio,
        }
    }

    pub fn capabilities(&self) -> SourceCapabilities {
        // DRM requires IQ access (the OFDM signal must reach the decoder
        // before any hardware AM demod). Any has_iq source gets it.
        let iq_modes = || {
            vec![
                Mode::USB,
                Mode::LSB,
                Mode::CW,
                Mode::CWR,
                Mode::AM,
                Mode::FM,
                Mode::DRM,
            ]
        };
        match self {
            SourceConfig::FdmDuo(_) => SourceCapabilities {
                kind: SourceKind::FdmDuo,
                has_iq: true,
                has_tx: true,
                has_hardware_cat: true,
                supported_demod_modes: iq_modes(),
            },
            SourceConfig::HackRf(_) => SourceCapabilities {
                kind: SourceKind::HackRf,
                has_iq: true,
                has_tx: true,
                has_hardware_cat: false,
                supported_demod_modes: iq_modes(),
            },
            SourceConfig::RspDx(_) => SourceCapabilities {
                kind: SourceKind::RspDx,
                has_iq: true,
                has_tx: false,
                has_hardware_cat: false,
                supported_demod_modes: iq_modes(),
            },
            SourceConfig::RtlSdr(_) => SourceCapabilities {
                kind: SourceKind::RtlSdr,
                has_iq: true,
                has_tx: false,
                has_hardware_cat: false,
                supported_demod_modes: iq_modes(),
            },
            SourceConfig::PortableRadio(_) => SourceCapabilities {
                kind: SourceKind::PortableRadio,
                has_iq: false,
                has_tx: false,
                has_hardware_cat: false,
                supported_demod_modes: vec![],
            },
        }
    }
}

/// Capability summary for the active source. Maps 1:1 to
/// `efd_proto::Capabilities`; kept as a local type so `efd-iq` owns the
/// per-backend truth table and the server just forwards it.
#[derive(Debug, Clone)]
pub struct SourceCapabilities {
    pub kind: SourceKind,
    pub has_iq: bool,
    pub has_tx: bool,
    pub has_hardware_cat: bool,
    pub supported_demod_modes: Vec<Mode>,
}
