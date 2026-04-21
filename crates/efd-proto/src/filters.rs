//! FDM-DUO reception-filter tables (manual §6.3.2 p.57).
//!
//! Lives in `efd-proto` so the server's `RF;` parser and the client's
//! BW-editor dropdown draw from the same canonical source. The tables
//! map the radio's `P2` filter index (as seen in `RF<P1><P2P2>;`) to a
//! UI-friendly label. `P1` is derived from the active mode via
//! [`kenwood_mode_char`].
//!
//! Software-only modes (SAM family, DSB, DRM) reuse the AM table —
//! the FDM-DUO stays in hardware-AM for those and the software demod
//! picks the sideband.

use crate::Mode;

/// One selectable filter option. `index` is the raw `P2` byte used on
/// the wire in `RF<P1><P2P2>;`; `label` is what the UI shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterOption {
    pub index: u8,
    pub label: &'static str,
}

const LSB_USB: &[FilterOption] = &[
    FilterOption { index: 0,  label: "1.6k" },
    FilterOption { index: 1,  label: "1.7k" },
    FilterOption { index: 2,  label: "1.8k" },
    FilterOption { index: 3,  label: "1.9k" },
    FilterOption { index: 4,  label: "2.0k" },
    FilterOption { index: 5,  label: "2.1k" },
    FilterOption { index: 6,  label: "2.2k" },
    FilterOption { index: 7,  label: "2.3k" },
    FilterOption { index: 8,  label: "2.4k" },
    FilterOption { index: 9,  label: "2.5k" },
    FilterOption { index: 10, label: "2.6k" },
    FilterOption { index: 11, label: "2.7k" },
    FilterOption { index: 12, label: "2.8k" },
    FilterOption { index: 13, label: "2.9k" },
    FilterOption { index: 14, label: "3.0k" },
    FilterOption { index: 15, label: "3.1k" },
    FilterOption { index: 16, label: "4.0k" },
    FilterOption { index: 17, label: "5.0k" },
    FilterOption { index: 18, label: "6.0k" },
    FilterOption { index: 19, label: "D300" },
    FilterOption { index: 20, label: "D600" },
    FilterOption { index: 21, label: "D1k"  },
];

// CW / CWR filters start at index 7 in the manual — indices 0–6 are
// reserved (`-` in the table).
const CW: &[FilterOption] = &[
    FilterOption { index: 7,  label: "100&4" },
    FilterOption { index: 8,  label: "100&3" },
    FilterOption { index: 9,  label: "100&2" },
    FilterOption { index: 10, label: "100&1" },
    FilterOption { index: 11, label: "100"   },
    FilterOption { index: 12, label: "300"   },
    FilterOption { index: 13, label: "500"   },
    FilterOption { index: 14, label: "1.0k"  },
    FilterOption { index: 15, label: "1.5k"  },
    FilterOption { index: 16, label: "2.6k"  },
];

const AM: &[FilterOption] = &[
    FilterOption { index: 0, label: "2.5k" },
    FilterOption { index: 1, label: "3.0k" },
    FilterOption { index: 2, label: "3.5k" },
    FilterOption { index: 3, label: "4.0k" },
    FilterOption { index: 4, label: "4.5k" },
    FilterOption { index: 5, label: "5.0k" },
    FilterOption { index: 6, label: "5.5k" },
    FilterOption { index: 7, label: "6.0k" },
];

const FM: &[FilterOption] = &[
    FilterOption { index: 0, label: "Narrow" },
    FilterOption { index: 1, label: "Wide"   },
    FilterOption { index: 2, label: "Data"   },
];

/// Filter options available for the given demod mode. Empty slice for
/// `Mode::Unknown`.
pub fn filters_for_mode(mode: Mode) -> &'static [FilterOption] {
    match mode {
        Mode::LSB | Mode::USB => LSB_USB,
        Mode::CW | Mode::CWR => CW,
        // AM family — hardware radio stays in AM; software demods
        // (SAM/SAMU/SAML/DSB/DRM) pick the sideband from its output.
        Mode::AM | Mode::DRM | Mode::SAM | Mode::SAMU | Mode::SAML | Mode::DSB => AM,
        Mode::FM => FM,
        Mode::Unknown => &[],
    }
}

/// Look up the UI label for a `(mode, index)` pair. Returns `None`
/// when the index isn't defined for that mode.
pub fn filter_label(mode: Mode, index: u8) -> Option<&'static str> {
    filters_for_mode(mode)
        .iter()
        .find(|f| f.index == index)
        .map(|f| f.label)
}

/// Kenwood `RF` / `MD` mode character — the `P1` byte in
/// `RF<P1><P2P2>;` and `MD<P1>;`. Software-only modes fall through to
/// AM (`5`), matching the FDM-DUO's native AM family.
pub fn kenwood_mode_char(mode: Mode) -> Option<char> {
    match mode {
        Mode::LSB => Some('1'),
        Mode::USB => Some('2'),
        Mode::CW => Some('3'),
        Mode::FM => Some('4'),
        Mode::AM | Mode::DRM | Mode::SAM | Mode::SAMU | Mode::SAML | Mode::DSB => Some('5'),
        Mode::CWR => Some('7'),
        Mode::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_usb_has_22_entries() {
        assert_eq!(filters_for_mode(Mode::LSB).len(), 22);
        assert_eq!(filters_for_mode(Mode::USB).len(), 22);
    }

    #[test]
    fn cw_starts_at_index_7() {
        let cw = filters_for_mode(Mode::CW);
        assert_eq!(cw.first().map(|f| f.index), Some(7));
        assert_eq!(cw.first().map(|f| f.label), Some("100&4"));
    }

    #[test]
    fn filter_label_roundtrip() {
        assert_eq!(filter_label(Mode::USB, 8), Some("2.4k"));
        assert_eq!(filter_label(Mode::AM, 0), Some("2.5k"));
        assert_eq!(filter_label(Mode::FM, 1), Some("Wide"));
        assert_eq!(filter_label(Mode::CW, 13), Some("500"));
        assert_eq!(filter_label(Mode::USB, 99), None);
    }

    #[test]
    fn software_modes_share_am_table() {
        for m in [Mode::SAM, Mode::SAMU, Mode::SAML, Mode::DSB, Mode::DRM] {
            assert_eq!(
                filters_for_mode(m).as_ptr(),
                filters_for_mode(Mode::AM).as_ptr()
            );
        }
    }

    #[test]
    fn kenwood_mode_char_values() {
        assert_eq!(kenwood_mode_char(Mode::LSB), Some('1'));
        assert_eq!(kenwood_mode_char(Mode::USB), Some('2'));
        assert_eq!(kenwood_mode_char(Mode::CW),  Some('3'));
        assert_eq!(kenwood_mode_char(Mode::FM),  Some('4'));
        assert_eq!(kenwood_mode_char(Mode::AM),  Some('5'));
        assert_eq!(kenwood_mode_char(Mode::CWR), Some('7'));
        assert_eq!(kenwood_mode_char(Mode::SAMU), Some('5'));
        assert_eq!(kenwood_mode_char(Mode::Unknown), None);
    }
}
