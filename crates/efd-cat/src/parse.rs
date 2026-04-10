use efd_proto::{Mode, Vfo};

/// Convert a Kenwood mode digit (from the IF response) to our Mode enum.
pub fn kenwood_mode(digit: u8) -> Mode {
    match digit {
        1 => Mode::LSB,
        2 => Mode::USB,
        3 => Mode::CW,
        4 => Mode::FM,
        5 => Mode::AM,
        7 => Mode::CWR,
        _ => Mode::Unknown,
    }
}

/// Convert our Mode to the Kenwood RF-command mode character.
pub fn mode_char(mode: Mode) -> Option<char> {
    match mode {
        Mode::LSB => Some('1'),
        Mode::USB => Some('2'),
        Mode::CW => Some('3'),
        Mode::FM => Some('4'),
        Mode::AM => Some('5'),
        Mode::CWR => Some('7'),
        Mode::Unknown => None,
    }
}

/// Parse an IF; response to extract frequency, mode, and VFO.
///
/// IF response format (Kenwood):
///   `IF` + freq(11) + step(4) + rit_offset(6) + rit(1) + xit(1) + _0_0(2) + _tx(1) + mode(1) + vfo(1) + ...`;`
///   Minimum 32 chars before the trailing `;`.
pub fn parse_if_response(response: &str) -> Option<(u64, Mode, Vfo)> {
    let s = response.trim();
    if s.len() < 32 || !s.starts_with("IF") {
        return None;
    }

    let freq: u64 = s[2..13].parse().ok()?;
    let mode_digit: u8 = s[29..30].parse().ok()?;
    let vfo_digit: u8 = s[30..31].parse().ok()?;

    let mode = kenwood_mode(mode_digit);
    let vfo = if vfo_digit == 0 { Vfo::A } else { Vfo::B };

    Some((freq, mode, vfo))
}

// ---------- filter bandwidth tables (per ELAD FDM-DUO manual) ----------

const FILTER_LSB_USB: &[&str] = &[
    "1.6k", "1.7k", "1.8k", "1.9k", "2.0k", "2.1k", "2.2k", "2.3k",
    "2.4k", "2.5k", "2.6k", "2.7k", "2.8k", "2.9k", "3.0k", "3.1k",
    "4.0k", "5.0k", "6.0k", "D300", "D600", "D1k",
];

const FILTER_CW: &[Option<&str>] = &[
    None, None, None, None, None, None, None,
    Some("100&4"), Some("100&3"), Some("100&2"), Some("100&1"),
    Some("100"), Some("300"), Some("500"),
    Some("1.0k"), Some("1.5k"), Some("2.6k"),
];

const FILTER_AM: &[&str] = &[
    "2.5k", "3.0k", "3.5k", "4.0k", "4.5k", "5.0k", "5.5k", "6.0k",
];

const FILTER_FM: &[&str] = &["Narrow", "Wide", "Data"];

/// Parse an RF; response to extract the filter bandwidth string.
///
/// RF response format: `RF` P1 P2P2 `;`  (e.g. `RF10808;`)
pub fn parse_rf_response(response: &str, mode: Mode) -> Option<String> {
    let s = response.trim();
    if s.len() < 6 || !s.starts_with("RF") {
        return None;
    }

    let p2: usize = s[3..5].parse().ok()?;

    let filter: Option<&str> = match mode {
        Mode::LSB | Mode::USB => FILTER_LSB_USB.get(p2).copied(),
        Mode::CW | Mode::CWR => FILTER_CW.get(p2).and_then(|o| *o),
        Mode::AM => FILTER_AM.get(p2).copied(),
        Mode::FM => FILTER_FM.get(p2).copied(),
        Mode::Unknown => None,
    };

    Some(filter.unwrap_or("?").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_if_basic() {
        // Standard IF response: freq=7100000, mode=USB(2), VFO=A(0)
        // Format: IF + freq(11) + 16 chars padding + mode(1) + vfo(1) + tail + ;
        // mode at index 29, vfo at index 30
        // IF(2) + freq(11) + padding(16) + mode(1)@29 + vfo(1)@30 + trail + ;
        let resp = "IF000071000000000000000000000200;";
        let (freq, mode, vfo) = parse_if_response(resp).unwrap();
        assert_eq!(freq, 7_100_000);
        assert_eq!(mode, Mode::USB);
        assert_eq!(vfo, Vfo::A);
    }

    #[test]
    fn parse_if_lsb_vfob() {
        let resp = "IF000142000000000000000000000110;";
        let (freq, mode, vfo) = parse_if_response(resp).unwrap();
        assert_eq!(freq, 14_200_000);
        assert_eq!(mode, Mode::LSB);
        assert_eq!(vfo, Vfo::B);
    }

    #[test]
    fn parse_if_too_short() {
        assert!(parse_if_response("IF001234;").is_none());
    }

    #[test]
    fn parse_if_wrong_prefix() {
        let resp = "FA00007100000;";
        assert!(parse_if_response(resp).is_none());
    }

    #[test]
    fn parse_rf_usb_2400() {
        let resp = "RF20808;";
        let bw = parse_rf_response(resp, Mode::USB).unwrap();
        assert_eq!(bw, "2.4k");
    }

    #[test]
    fn parse_rf_cw_500() {
        let resp = "RF31300;";
        let bw = parse_rf_response(resp, Mode::CW).unwrap();
        assert_eq!(bw, "500");
    }

    #[test]
    fn parse_rf_am() {
        let resp = "RF50300;";
        let bw = parse_rf_response(resp, Mode::AM).unwrap();
        assert_eq!(bw, "4.0k");
    }

    #[test]
    fn kenwood_mode_mapping() {
        assert_eq!(kenwood_mode(1), Mode::LSB);
        assert_eq!(kenwood_mode(2), Mode::USB);
        assert_eq!(kenwood_mode(3), Mode::CW);
        assert_eq!(kenwood_mode(4), Mode::FM);
        assert_eq!(kenwood_mode(5), Mode::AM);
        assert_eq!(kenwood_mode(7), Mode::CWR);
        assert_eq!(kenwood_mode(0), Mode::Unknown);
        assert_eq!(kenwood_mode(9), Mode::Unknown);
    }
}
