use efd_proto::{CatCommand, Mode, Vfo};

/// Build a CAT command to set the VFO A frequency.
pub fn set_freq(hz: u64) -> CatCommand {
    CatCommand {
        raw: format!("FA{:011};", hz),
    }
}

/// Build a CAT command to set the operating mode.
pub fn set_mode(mode: Mode) -> Option<CatCommand> {
    let digit = match mode {
        Mode::LSB => 1,
        Mode::USB => 2,
        Mode::CW => 3,
        Mode::FM => 4,
        Mode::AM => 5,
        Mode::CWR => 7,
        Mode::Unknown => return None,
    };
    Some(CatCommand {
        raw: format!("MD{digit};"),
    })
}

/// Build a CAT command to select VFO A or B.
pub fn set_vfo(vfo: Vfo) -> CatCommand {
    let digit = match vfo {
        Vfo::A => 0,
        Vfo::B => 1,
    };
    CatCommand {
        raw: format!("FR{digit};"),
    }
}
