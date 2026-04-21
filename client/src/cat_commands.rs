use efd_proto::{AgcMode, CatCommand, Mode};

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

/// Build a CAT command to set the reception filter for a given mode.
///
/// Wire form per manual §6.3.2 p.57: `RF<P1><P2P2>;`, where `P1` is
/// the Kenwood mode digit (from [`efd_proto::kenwood_mode_char`]) and
/// `P2P2` is the 2-digit filter index. Returns `None` when the mode
/// has no Kenwood digit (e.g. `Mode::Unknown`).
pub fn set_filter(mode: Mode, index: u8) -> Option<CatCommand> {
    let p1 = efd_proto::kenwood_mode_char(mode)?;
    Some(CatCommand {
        raw: format!("RF{p1}{index:02};"),
    })
}

/// Build a CAT command to set the AGC threshold (0–10).
pub fn set_agc_threshold(value: u8) -> CatCommand {
    let v = value.min(10);
    CatCommand {
        raw: format!("TH{v:02};"),
    }
}

/// Build the CAT commands needed to set AGC speed on the FDM-DUO.
///
/// The native surface (manual §6.3.2) is two commands:
///   - `GC0;` / `GC1;` picks auto (AGC) vs. manual gain.
///   - `GS 0 P2 P2 P2 ;` sets the speed when auto:
///         `000` slow, `001` medium, `002` fast.
///
/// Note: the manual's GS table shows only two P2 cells, but the
/// radio's own reply to a `GS0;` read uses three digits
/// (e.g. `GS0001;`). Bench-verified on firmware Rev 2.13. The
/// two-digit variant is silently ignored, which is what produced
/// the "speed doesn't take effect" report.
///
/// `Off` emits just `GC1;` (switch to manual gain — AGC bypassed);
/// the three speeds emit `GC0;` followed by the matching `GS`. The
/// legacy Kenwood-style `GTnnn;` is a compatibility no-op on the
/// FDM-DUO and is not used.
pub fn set_agc_mode(mode: AgcMode) -> Vec<CatCommand> {
    match mode {
        AgcMode::Off => vec![CatCommand { raw: "GC1;".into() }],
        speed => {
            let p2 = match speed {
                AgcMode::Slow => 0,
                AgcMode::Medium => 1,
                AgcMode::Fast => 2,
                AgcMode::Off => unreachable!(),
            };
            vec![
                CatCommand { raw: "GC0;".into() },
                CatCommand { raw: format!("GS0{p2:03};") },
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raws(cmds: Vec<CatCommand>) -> Vec<String> {
        cmds.into_iter().map(|c| c.raw).collect()
    }

    #[test]
    fn threshold_format() {
        assert_eq!(set_agc_threshold(0).raw, "TH00;");
        assert_eq!(set_agc_threshold(5).raw, "TH05;");
        assert_eq!(set_agc_threshold(10).raw, "TH10;");
        // Values above 10 clamp to 10.
        assert_eq!(set_agc_threshold(42).raw, "TH10;");
    }

    #[test]
    fn agc_mode_off_is_single_gc1() {
        assert_eq!(raws(set_agc_mode(AgcMode::Off)), vec!["GC1;"]);
    }

    #[test]
    fn set_filter_format() {
        assert_eq!(set_filter(Mode::USB, 8).unwrap().raw, "RF208;");
        assert_eq!(set_filter(Mode::CW, 13).unwrap().raw, "RF313;");
        assert_eq!(set_filter(Mode::AM, 0).unwrap().raw, "RF500;");
        assert_eq!(set_filter(Mode::FM, 1).unwrap().raw, "RF401;");
        // Software-only modes park at AM (P1 = '5').
        assert_eq!(set_filter(Mode::SAM, 3).unwrap().raw, "RF503;");
        assert!(set_filter(Mode::Unknown, 0).is_none());
    }

    #[test]
    fn agc_mode_speeds_emit_gc0_then_gs() {
        assert_eq!(
            raws(set_agc_mode(AgcMode::Slow)),
            vec!["GC0;", "GS0000;"]
        );
        assert_eq!(
            raws(set_agc_mode(AgcMode::Medium)),
            vec!["GC0;", "GS0001;"]
        );
        assert_eq!(
            raws(set_agc_mode(AgcMode::Fast)),
            vec!["GC0;", "GS0002;"]
        );
    }
}

