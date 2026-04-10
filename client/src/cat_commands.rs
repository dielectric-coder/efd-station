use efd_proto::CatCommand;

/// Build a CAT command to set the VFO A frequency.
pub fn set_freq(hz: u64) -> CatCommand {
    CatCommand {
        raw: format!("FA{:011};", hz),
    }
}
