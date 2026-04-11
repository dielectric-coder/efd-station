use efd_proto::CatCommand;

/// Build a CAT command to set the VFO A frequency.
pub fn set_freq(hz: u64) -> CatCommand {
    CatCommand {
        raw: format!("FA{:011};", hz),
    }
}

/// Build a CAT command to set the AGC threshold (0–10).
pub fn set_agc_threshold(value: u8) -> CatCommand {
    let v = value.min(10);
    CatCommand {
        raw: format!("TH{v:02};"),
    }
}

