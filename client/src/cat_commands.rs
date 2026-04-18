use efd_proto::CatCommand;

/// Build a CAT command to set the VFO A frequency.
pub fn set_freq(hz: u64) -> CatCommand {
    CatCommand {
        raw: format!("FA{:011};", hz),
    }
}

/// Build a CAT command to set the operating mode (Kenwood MD command).
///
/// Software-only modes (SAM, SAMU, SAML, DSB) have no hardware
/// equivalent on the FDM-DUO; we park the radio in AM and let the
/// software demod pick the sideband. Same convention DRM uses.
pub fn set_mode(mode: efd_proto::Mode) -> Option<CatCommand> {
    let digit = match mode {
        efd_proto::Mode::LSB => '1',
        efd_proto::Mode::USB => '2',
        efd_proto::Mode::CW => '3',
        efd_proto::Mode::FM => '4',
        efd_proto::Mode::AM
        | efd_proto::Mode::DRM
        | efd_proto::Mode::SAM
        | efd_proto::Mode::SAMU
        | efd_proto::Mode::SAML
        | efd_proto::Mode::DSB => '5',
        efd_proto::Mode::CWR => '7',
        efd_proto::Mode::Unknown => return None,
    };
    Some(CatCommand {
        raw: format!("MD{digit};"),
    })
}

/// Build a CAT command to set the AGC threshold (0–10).
pub fn set_agc_threshold(value: u8) -> CatCommand {
    let v = value.min(10);
    CatCommand {
        raw: format!("TH{v:02};"),
    }
}

