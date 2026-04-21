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
///
/// Software-only modes (SAM, SAMU, SAML, DSB) have no FDM-DUO hardware
/// equivalent; the radio stays in AM while the software demod does the
/// work, mirroring the DRM convention.
pub fn mode_char(mode: Mode) -> Option<char> {
    match mode {
        Mode::LSB => Some('1'),
        Mode::USB => Some('2'),
        Mode::CW => Some('3'),
        Mode::FM => Some('4'),
        Mode::AM
        | Mode::DRM
        | Mode::SAM
        | Mode::SAMU
        | Mode::SAML
        | Mode::DSB => Some('5'),
        Mode::CWR => Some('7'),
        Mode::Unknown => None,
    }
}

/// Parsed fields from the IF; response.
#[derive(Debug, Clone)]
pub struct IfResponse {
    pub freq_hz: u64,
    pub mode: Mode,
    pub vfo: Vfo,
    pub tx: bool,
}

/// Parse an IF; response to extract frequency, mode, VFO, and TX state.
///
/// IF response format (Kenwood):
///   `IF` + freq(11) + step(4) + rit_offset(5) + rit(1) + xit(1) + _0(1) + _0(1) + tx(1) + mode(1) + vfo(1) + ...`;`
///   tx at index 28, mode at index 29, vfo at index 30.
///   Minimum 32 chars before the trailing `;`.
pub fn parse_if_response(response: &str) -> Option<IfResponse> {
    let s = response.trim();
    if !s.starts_with("IF") {
        return None;
    }

    // `.get(..)` instead of raw indexing so a short frame or non-ASCII
    // byte at a char boundary returns None rather than panicking.
    let freq_hz: u64 = s.get(2..13)?.parse().ok()?;
    let tx_digit: u8 = s.get(28..29)?.parse().ok().unwrap_or(0);
    let mode_digit: u8 = s.get(29..30)?.parse().ok()?;
    let vfo_digit: u8 = s.get(30..31)?.parse().ok()?;

    let mode = kenwood_mode(mode_digit);
    let vfo = if vfo_digit == 0 { Vfo::A } else { Vfo::B };
    let tx = tx_digit == 1;

    Some(IfResponse {
        freq_hz,
        mode,
        vfo,
        tx,
    })
}

/// Compatibility wrapper returning (freq, mode, vfo) tuple.
pub fn parse_if_response_tuple(response: &str) -> Option<(u64, Mode, Vfo)> {
    let r = parse_if_response(response)?;
    Some((r.freq_hz, r.mode, r.vfo))
}

/// Parse an SM; (S-meter) response. Returns S-meter value in dBm.
///
/// SM response format: `SM` + P1(1) + P2P2P2P2(4) + `;`
/// FDM-DUO SM scale (from manual):
///   0000=S0, 0002=S1, 0003=S2, 0004=S3, 0005=S4,
///   0006=S5, 0008=S6, 0009=S7, 0010=S8, 0011=S9,
///   0012=S9+10, 0014=S9+20, 0016=S9+30,
///   0018=S9+40, 0020=S9+50, 0022=S9+60
pub fn parse_sm_response(response: &str) -> Option<f32> {
    let s = response.trim();
    if s.len() < 7 || !s.starts_with("SM") {
        return None;
    }
    let reading: u16 = s[3..7].parse().ok()?;

    // Map the FDM-DUO discrete SM values to dBm.
    // S0=-127, S9=-73 (54dB range over 11 steps), S9+60=-13
    let dbm = if reading <= 11 {
        // S0 to S9: 0..11 → -127..-73
        -127.0 + (reading as f32 / 11.0) * 54.0
    } else {
        // S9+ : 11..22 → -73..-13 (60dB over 11 steps)
        -73.0 + ((reading - 11) as f32 / 11.0) * 60.0
    };
    Some(dbm.clamp(-127.0, 0.0))
}

/// Parse an RI; (RSSI) response. Returns RSSI in dBm.
///
/// RI response format: `RI` + P1(sign: +/-/!) + P2P2P2P2P2(5 digits) + `;`
/// P1: '-' negative, '+' positive, '!' unreliable
pub fn parse_ri_response(response: &str) -> Option<f32> {
    let s = response.trim();
    if s.len() < 8 || !s.starts_with("RI") {
        return None;
    }
    let sign = s.as_bytes()[2];
    if sign == b'!' {
        return None; // unreliable
    }
    let value: f32 = s[3..8].trim().parse().ok()?;
    if sign == b'-' {
        Some(-value)
    } else {
        Some(value)
    }
}

/// Parse a TH; (AGC threshold) response. Returns threshold value (0–10).
///
/// TH response format: `TH` + P1P1(2 digits, 00–10) + `;`
pub fn parse_th_response(response: &str) -> Option<u8> {
    let s = response.trim();
    if s.len() < 5 || !s.starts_with("TH") {
        return None;
    }
    let value: u8 = s[2..4].parse().ok()?;
    if value <= 10 { Some(value) } else { None }
}

/// Parse a single-digit on/off bit out of a `<XX>n;` response, where the
/// response is exactly four characters (two-letter prefix, one digit, `;`).
/// Treats any non-zero digit as "on".
fn parse_bit_response(response: &str, prefix: &str) -> Option<bool> {
    let s = response.trim();
    if !s.starts_with(prefix) || !s.ends_with(';') {
        return None;
    }
    // Payload is everything between prefix and trailing ';'.
    let payload = &s[prefix.len()..s.len() - 1];
    if payload.is_empty() {
        return None;
    }
    // Tolerate longer payloads (e.g. NR2;, NB1;) — first digit is the on/off bit.
    let first = payload.chars().next()?;
    let n = first.to_digit(10)?;
    Some(n != 0)
}

/// Parse an RA; (attenuator) response. `RA00;` is off; any other value is on.
/// FDM-DUO uses Kenwood-compatible RA, with stepped values on some firmwares.
pub fn parse_ra_response(response: &str) -> Option<bool> {
    let s = response.trim();
    if !s.starts_with("RA") || !s.ends_with(';') {
        return None;
    }
    let payload = &s[2..s.len() - 1];
    if payload.is_empty() || !payload.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(payload.chars().any(|c| c != '0'))
}

/// Parse an LP; (50 MHz low-pass filter) response. `LP0;` off, `LPx;` on.
pub fn parse_lp_response(response: &str) -> Option<bool> {
    parse_bit_response(response, "LP")
}

/// Parse an NR; (noise reduction) response. `NR0;` off, `NRx;` on at level x.
pub fn parse_nr_response(response: &str) -> Option<bool> {
    parse_bit_response(response, "NR")
}

/// Parse an NB; (noise blanker) response. `NB0;` off, `NBx;` on at level x.
pub fn parse_nb_response(response: &str) -> Option<bool> {
    parse_bit_response(response, "NB")
}

/// Parse a `GC;` (active gain control) response.
///
/// `GC0;` = auto (AGC engaged). `GC1;` = manual gain (AGC off).
/// Returns `true` when AGC is engaged.
pub fn parse_gc_response(response: &str) -> Option<bool> {
    let s = response.trim();
    if !s.starts_with("GC") || !s.ends_with(';') {
        return None;
    }
    match &s[2..s.len() - 1] {
        "0" => Some(true),
        "1" => Some(false),
        _ => None,
    }
}

/// Parse a `GS;` (control gain settings) response. Format on the
/// FDM-DUO firmware is `GS P1 P2 P2 P2 ;` — P1 selects the active
/// gain mode (`0`=auto, `1`=manual), P2P2P2 is a three-digit value:
///   - `P1='0'` (auto/AGC): `000` slow, `001` medium, `002` fast.
///   - `P1='1'` (manual):   `000` OFF, `001`..`010` manual gain.
///
/// (Bench-verified on firmware Rev 2.13 — the manual's table shows
/// only two P2 cells, but the radio's own answer uses three.)
///
/// Returns `(is_auto, p2)`; the caller combines it with the `GC;`
/// answer to produce an [`AgcMode`].
pub fn parse_gs_response(response: &str) -> Option<(bool, u16)> {
    let s = response.trim();
    if !s.starts_with("GS") || !s.ends_with(';') {
        return None;
    }
    let payload = &s[2..s.len() - 1];
    if payload.len() != 4 {
        return None;
    }
    let (p1, p2_str) = payload.split_at(1);
    let is_auto = match p1 {
        "0" => true,
        "1" => false,
        _ => return None,
    };
    let p2: u16 = p2_str.parse().ok()?;
    Some((is_auto, p2))
}

/// Combine a `GC;` + `GS;` answer pair into an [`AgcMode`]. `GC1;`
/// (manual gain) always maps to [`AgcMode::Off`] regardless of `GS`.
/// `GC0;` + `GS 0 PPP;` maps the three-digit P2 field: `000`→Slow,
/// `001`→Medium, `002`→Fast. Unknown values fall back to
/// [`AgcMode::Slow`].
pub fn gs_to_agc_mode(gc_auto: bool, gs_p2: u16) -> crate::AgcMode {
    use crate::AgcMode;
    if !gc_auto {
        return AgcMode::Off;
    }
    match gs_p2 {
        0 => AgcMode::Slow,
        1 => AgcMode::Medium,
        2 => AgcMode::Fast,
        _ => AgcMode::Slow,
    }
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
        // AM/DRM and the software-only AM-family modes (SAM*, DSB) all
        // share the radio's AM filter table — hardware stays in AM, the
        // software demod picks the sideband.
        Mode::AM | Mode::DRM | Mode::SAM | Mode::SAMU | Mode::SAML | Mode::DSB => {
            FILTER_AM.get(p2).copied()
        }
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
        // IF(2) + freq(11) + padding(16) + tx(1)@28 + mode(1)@29 + vfo(1)@30 + trail + ;
        let resp = "IF000071000000000000000000000200;";
        let r = parse_if_response(resp).unwrap();
        assert_eq!(r.freq_hz, 7_100_000);
        assert_eq!(r.mode, Mode::USB);
        assert_eq!(r.vfo, Vfo::A);
        assert!(!r.tx);
    }

    #[test]
    fn parse_if_lsb_vfob() {
        let resp = "IF000142000000000000000000000110;";
        let r = parse_if_response(resp).unwrap();
        assert_eq!(r.freq_hz, 14_200_000);
        assert_eq!(r.mode, Mode::LSB);
        assert_eq!(r.vfo, Vfo::B);
    }

    #[test]
    fn parse_if_tx_state() {
        // tx=1 at position 28
        let resp = "IF000071000000000000000000001200;";
        let r = parse_if_response(resp).unwrap();
        assert!(r.tx);
    }

    #[test]
    fn parse_sm_s9() {
        // S9 = reading 0011 per FDM-DUO manual
        let resp = "SM00011;";
        let db = parse_sm_response(resp).unwrap();
        assert!((db - (-73.0)).abs() < 0.1, "S9 should be -73 dBm, got {db}");
    }

    #[test]
    fn parse_sm_s0() {
        let resp = "SM00000;";
        let db = parse_sm_response(resp).unwrap();
        assert!((db - (-127.0)).abs() < 0.1);
    }

    #[test]
    fn parse_sm_s9_plus_60() {
        // S9+60 = reading 0022
        let resp = "SM00022;";
        let db = parse_sm_response(resp).unwrap();
        assert!((db - (-13.0)).abs() < 0.1, "S9+60 should be -13 dBm, got {db}");
    }

    #[test]
    fn parse_ri_negative() {
        let resp = "RI-00085;";
        let db = parse_ri_response(resp).unwrap();
        assert!((db - (-85.0)).abs() < 0.1);
    }

    #[test]
    fn parse_ri_unreliable() {
        let resp = "RI!00000;";
        assert!(parse_ri_response(resp).is_none());
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
    fn parse_ra_off_on() {
        assert_eq!(parse_ra_response("RA00;"), Some(false));
        assert_eq!(parse_ra_response("RA01;"), Some(true));
        assert_eq!(parse_ra_response("RA00000;"), Some(false));
        assert_eq!(parse_ra_response("RA00010;"), Some(true));
        assert_eq!(parse_ra_response("BAD;"), None);
    }

    #[test]
    fn parse_lp_nr_nb_bits() {
        assert_eq!(parse_lp_response("LP0;"), Some(false));
        assert_eq!(parse_lp_response("LP1;"), Some(true));
        assert_eq!(parse_nr_response("NR0;"), Some(false));
        assert_eq!(parse_nr_response("NR2;"), Some(true));
        assert_eq!(parse_nb_response("NB0;"), Some(false));
        assert_eq!(parse_nb_response("NB1;"), Some(true));
        assert_eq!(parse_nr_response("foo"), None);
    }

    #[test]
    fn parse_gc_gs_and_agc_mode() {
        use efd_proto::AgcMode;
        // GC; picks auto vs manual.
        assert_eq!(parse_gc_response("GC0;"), Some(true));
        assert_eq!(parse_gc_response("GC1;"), Some(false));
        assert_eq!(parse_gc_response("GCx;"), None);
        assert_eq!(parse_gc_response("foo"), None);

        // GS; carries the (auto, P2) pair — 4-byte payload
        // (P1 + 3-digit P2) per observed firmware behavior.
        assert_eq!(parse_gs_response("GS0000;"), Some((true, 0)));
        assert_eq!(parse_gs_response("GS0001;"), Some((true, 1)));
        assert_eq!(parse_gs_response("GS0002;"), Some((true, 2)));
        assert_eq!(parse_gs_response("GS1010;"), Some((false, 10)));
        assert_eq!(parse_gs_response("GS000;"), None); // old 3-byte payload
        assert_eq!(parse_gs_response("GSxxxx;"), None);

        // Combining yields AgcMode.
        assert_eq!(gs_to_agc_mode(true, 0), AgcMode::Slow);
        assert_eq!(gs_to_agc_mode(true, 1), AgcMode::Medium);
        assert_eq!(gs_to_agc_mode(true, 2), AgcMode::Fast);
        // Manual gain (GC1;) always reads back as Off regardless of
        // the P2 value, since the AGC stage is bypassed.
        assert_eq!(gs_to_agc_mode(false, 0), AgcMode::Off);
        assert_eq!(gs_to_agc_mode(false, 5), AgcMode::Off);
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
