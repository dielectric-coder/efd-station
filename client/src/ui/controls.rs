use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use efd_proto::{
    Capabilities, ClientMsg, DrmStatus, Mode, Ptt, RadioState, RecKind, RecordingStatus,
    SourceClass, StartRecording, StateSnapshot,
};
use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Box as GtkBox, Button, DropDown, Entry, Label, LevelBar, Orientation,
    Scale, StringList, ToggleButton,
};
use tokio::sync::mpsc;

use crate::audio::AudioPlayer;
use crate::cat_commands;
use crate::sdr_params::{self, SdrParams};

// ---------------------------------------------------------------------------
// Display bar (top) — read-only status: VFO, freq, mode, BW, S-meter, RX/TX
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct DisplayBar {
    container: GtkBox,
    freq_label: Label,
    mode_label: Label,
    vfo_label: Label,
    bw_label: Label,
    smeter: LevelBar,
    smeter_label: Label,
    tx_label: Label,
    /// Currently-selected source indicator (disp0-left): "SRC: AUD" or
    /// "SRC: IQ". Updated from the control bar's source-toggle handler.
    selected_src_label: Label,
    /// AUD / IQ availability indicators (disp1-left). Each is styled
    /// either active (`.app-mode`) or greyed-out (`.app-mode-disabled`)
    /// based on the corresponding capability flag. Both hidden when
    /// neither source is available; `no_device_label` takes over.
    aud_avail_label: Label,
    iq_avail_label: Label,
    no_device_label: Label,
    /// First DRM info line — mode/bandwidth/modulation/services.
    drm_line1: Label,
    /// Second DRM info line — SNR/WMER/lock flags.
    drm_line2: Label,
    /// Scrolling decoded-text output (CW / RTTY / PSK / …). Shown in
    /// `disp2-center` alongside the DRM info lines; each incoming
    /// `DecodedText` message appends a line, keeping at most
    /// `DECODED_LINES_KEPT` so the widget doesn't grow unbounded.
    decoded_text_label: Label,
    decoded_lines: Rc<RefCell<std::collections::VecDeque<String>>>,
    prev: RefCell<Option<CachedState>>,
}

/// Rolling log cap for the decoded-text view. 6 fits nicely in the
/// `disp2-center` cell at the default font size and matches the
/// diagram's "QST DE WA1W ... 15 WPM TEST" style of recent-activity
/// display.
const DECODED_LINES_KEPT: usize = 6;

#[derive(Clone, PartialEq)]
struct CachedState {
    freq_hz: u64,
    mode: String,
    vfo: String,
    filter_bw: String,
    s_reading: u16,
    tx: bool,
}

impl DisplayBar {
    pub fn new() -> Self {
        // Outer vertical container holds three rows. Each row has left /
        // center / right slots so cross-row alignment matches the
        // drawio wireframe (docs/client-sdr-UI.drawio).
        let container = GtkBox::new(Orientation::Vertical, 2);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);
        container.set_hexpand(true);

        let (row0, disp0_left, disp0_center, disp0_right) = make_lcr_row();
        container.append(&row0);
        let (row1, disp1_left, disp1_center, _) = make_lcr_row();
        container.append(&row1);
        let (row2, _, disp2_center, _) = make_lcr_row();
        container.append(&row2);

        // disp0-left: currently-selected source ("SRC: AUD" / "SRC: IQ").
        let selected_src_label = Label::new(Some("SRC: AUD"));
        selected_src_label.add_css_class("monospace");
        selected_src_label.add_css_class("app-mode");
        selected_src_label.set_width_chars(8);
        selected_src_label.set_xalign(0.0);
        disp0_left.append(&selected_src_label);

        // disp0-center: VFO, freq, mode, BW, S-meter (center-justified).
        let vfo_label = Label::new(Some("VFO A"));
        vfo_label.add_css_class("monospace");
        vfo_label.set_width_chars(5);
        vfo_label.set_xalign(0.5);
        disp0_center.append(&vfo_label);

        let freq_label = Label::new(Some("--- Hz"));
        freq_label.add_css_class("monospace");
        freq_label.set_width_chars(16);
        freq_label.set_xalign(0.5);
        freq_label.set_markup("<span font='18' weight='bold'>--- Hz</span>");
        disp0_center.append(&freq_label);

        let mode_label = Label::new(Some("---"));
        mode_label.add_css_class("monospace");
        mode_label.set_width_chars(5);
        mode_label.set_xalign(0.5);
        disp0_center.append(&mode_label);

        let bw_label = Label::new(Some("BW: ---"));
        bw_label.add_css_class("monospace");
        bw_label.set_width_chars(10);
        bw_label.set_xalign(0.5);
        disp0_center.append(&bw_label);

        let smeter_box = GtkBox::new(Orientation::Horizontal, 4);
        smeter_box.set_valign(Align::Center);
        let smeter_title = Label::new(Some("S:"));
        smeter_box.append(&smeter_title);

        let smeter = LevelBar::new();
        smeter.set_min_value(0.0);
        smeter.set_max_value(30.0);
        smeter.set_value(0.0);
        smeter.set_width_request(100);
        smeter.set_height_request(8);
        smeter.set_valign(Align::Center);
        smeter_box.append(&smeter);

        let smeter_label = Label::new(Some("S0"));
        smeter_label.add_css_class("monospace");
        smeter_label.set_width_chars(6);
        smeter_label.set_xalign(0.5);
        smeter_box.append(&smeter_label);
        disp0_center.append(&smeter_box);

        // disp0-right: RX/TX indicator, right-justified.
        let tx_label = Label::new(Some("RX"));
        tx_label.add_css_class("monospace");
        tx_label.add_css_class("tx-rx-rx");
        tx_label.set_width_chars(2);
        tx_label.set_xalign(1.0);
        tx_label.set_halign(Align::End);
        disp0_right.append(&tx_label);

        // disp1-left: two availability indicators (AUD + IQ), plus a
        // NO-DEVICE label shown when neither source is present.
        let aud_avail_label = Label::new(Some("AUD"));
        aud_avail_label.add_css_class("monospace");
        aud_avail_label.add_css_class("app-mode");
        aud_avail_label.set_xalign(0.0);
        disp1_left.append(&aud_avail_label);

        let iq_avail_label = Label::new(Some("IQ"));
        iq_avail_label.add_css_class("monospace");
        iq_avail_label.add_css_class("app-mode");
        iq_avail_label.set_xalign(0.0);
        disp1_left.append(&iq_avail_label);

        let no_device_label = Label::new(Some("NO-DEVICE"));
        no_device_label.add_css_class("monospace");
        no_device_label.add_css_class("app-mode-warn");
        no_device_label.set_xalign(0.0);
        no_device_label.set_visible(false);
        disp1_left.append(&no_device_label);

        // disp1-center / disp2-center: DRM info lines (center-justified).
        // Mode-agnostic rows — today carry DRM decoder status; future
        // modes (RIT/XIT/DNR/DNF/NB readouts, etc.) reuse them via
        // `update_drm` / `clear_extras` without widget-tree changes.
        let drm_line1 = Label::new(None);
        drm_line1.add_css_class("monospace");
        drm_line1.set_xalign(0.5);
        disp1_center.append(&drm_line1);

        let drm_line2 = Label::new(None);
        drm_line2.add_css_class("monospace");
        drm_line2.set_xalign(0.5);
        disp2_center.append(&drm_line2);

        // disp2-center also carries the decoded-text log (phase 5a).
        // Stays below drm_line2 so DRM sessions aren't crowded;
        // renders empty until the first `DecodedText` arrives.
        let decoded_text_label = Label::new(None);
        decoded_text_label.add_css_class("monospace");
        decoded_text_label.set_xalign(0.5);
        decoded_text_label.set_wrap(true);
        decoded_text_label.set_lines(DECODED_LINES_KEPT as i32);
        disp2_center.append(&decoded_text_label);
        let decoded_lines = Rc::new(RefCell::new(std::collections::VecDeque::with_capacity(
            DECODED_LINES_KEPT,
        )));

        Self {
            container,
            freq_label,
            mode_label,
            vfo_label,
            bw_label,
            smeter,
            smeter_label,
            tx_label,
            selected_src_label,
            aud_avail_label,
            iq_avail_label,
            no_device_label,
            drm_line1,
            drm_line2,
            decoded_text_label,
            decoded_lines,
            prev: RefCell::new(None),
        }
    }

    /// Append a decoded-text line to the disp2-center log, capping
    /// history at `DECODED_LINES_KEPT` so the widget doesn't grow
    /// unbounded under a chatty decoder.
    pub fn push_decoded(&self, decoder: efd_proto::DecoderKind, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let mut lines = self.decoded_lines.borrow_mut();
        while lines.len() >= DECODED_LINES_KEPT {
            lines.pop_front();
        }
        // Tag with decoder kind so CW / RTTY / PSK etc. are visually
        // distinct. Keeps the log scannable when multiple decoders
        // are active simultaneously.
        lines.push_back(format!("[{:?}] {}", decoder, text.trim()));
        let joined: Vec<&str> = lines.iter().map(String::as_str).collect();
        self.decoded_text_label.set_text(&joined.join("\n"));
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Update the `SRC: AUD` / `SRC: IQ` indicator in disp0-left.
    pub fn set_selected_source(&self, is_iq: bool) {
        self.selected_src_label
            .set_text(if is_iq { "SRC: IQ" } else { "SRC: AUD" });
    }

    /// Paint the AUD/IQ availability indicators in disp1-left.
    /// Each side is greyed out (`.app-mode-disabled`) when its
    /// capability flag is false. When both are false, both are hidden
    /// and `NO-DEVICE` appears instead.
    pub fn set_source_availability(&self, has_aud: bool, has_iq: bool) {
        let any = has_aud || has_iq;
        self.aud_avail_label.set_visible(any);
        self.iq_avail_label.set_visible(any);
        self.no_device_label.set_visible(!any);
        if !any {
            return;
        }
        apply_avail_style(&self.aud_avail_label, has_aud);
        apply_avail_style(&self.iq_avail_label, has_iq);
    }

    /// Optimistic frequency update (before radio confirms).
    pub fn set_freq_immediate(&self, hz: u64) {
        let freq = format_freq(hz);
        self.freq_label
            .set_markup(&format!("<span font='18' weight='bold'>{freq}</span>"));
    }

    pub fn update(&self, state: &RadioState) {
        let s_reading = db_to_s_reading(state.s_meter_db);
        let mode_str = format!("{:?}", state.mode);
        let vfo_str = format!("VFO {:?}", state.vfo);

        let new_state = CachedState {
            freq_hz: state.freq_hz,
            mode: mode_str.clone(),
            vfo: vfo_str.clone(),
            filter_bw: state.filter_bw.clone(),
            s_reading: (s_reading * 10.0) as u16,
            tx: state.tx,
        };

        let mut prev = self.prev.borrow_mut();
        if prev.as_ref() == Some(&new_state) {
            return;
        }
        *prev = Some(new_state);

        let freq = format_freq(state.freq_hz);
        self.freq_label
            .set_markup(&format!("<span font='18' weight='bold'>{freq}</span>"));
        self.mode_label.set_text(&mode_str);
        self.vfo_label.set_text(&vfo_str);
        self.bw_label
            .set_text(&format!("BW: {}", state.filter_bw));
        self.smeter.set_value(s_reading as f64);
        self.smeter_label.set_text(&s_reading_to_string(s_reading));

        if state.tx {
            self.tx_label.remove_css_class("tx-rx-rx");
            self.tx_label.add_css_class("tx-rx-tx");
            self.tx_label.set_text("TX");
        } else {
            self.tx_label.remove_css_class("tx-rx-tx");
            self.tx_label.add_css_class("tx-rx-rx");
            self.tx_label.set_text("RX");
        }
    }

    /// Fill the two extra rows with the latest DRM decoder status.
    /// Leaves rows untouched otherwise — callers should set the
    /// mode-appropriate content and invoke `clear_extras` when leaving
    /// that mode.
    pub fn update_drm(&self, s: &DrmStatus) {
        let mode = s.robustness_mode.as_deref().unwrap_or("---");
        let bw = s
            .bandwidth_khz
            .map(|v| format!("{v} kHz"))
            .unwrap_or_else(|| "---".into());
        let msc_mod = s.msc_mode.as_deref().unwrap_or("---");
        let audio = s.num_audio_services;
        let data = s.num_data_services;
        self.drm_line1.set_text(&format!(
            "DRM Mode {mode} · {bw} · {msc_mod} · Audio:{audio} Data:{data}"
        ));

        let snr = s
            .snr_db
            .map(|v| format!("{v:.1} dB"))
            .unwrap_or_else(|| "---".into());
        let wmer = s
            .wmer_db
            .map(|v| format!("{v:.1} dB"))
            .unwrap_or_else(|| "---".into());
        let mark = |ok: bool| if ok { "✓" } else { "✗" };
        self.drm_line2.set_text(&format!(
            "SNR {snr} · WMER {wmer} · FAC {} · SDC {} · MSC {}",
            mark(s.fac_ok),
            mark(s.sdc_ok),
            mark(s.msc_ok),
        ));
    }

    /// Prime the two rows with mode-appropriate placeholders when the
    /// client enters DRM mode before any DrmStatus has arrived. Future
    /// modes (RIT/XIT/DNR/DNF/NB readouts, etc.) should add their own
    /// `prime_*` method that writes a similar placeholder header.
    pub fn prime_drm_placeholders(&self) {
        self.drm_line1
            .set_text("DRM Mode --- · --- · --- · Audio:0 Data:0");
        self.drm_line2
            .set_text("SNR --- · WMER --- · FAC ✗ · SDC ✗ · MSC ✗");
    }

    /// Blank the two extra rows. Called on mode changes that leave the
    /// rows without content — the rows themselves stay visible so the
    /// layout doesn't shift, they just go blank until the next mode
    /// claims them.
    pub fn clear_extras(&self) {
        self.drm_line1.set_text("");
        self.drm_line2.set_text("");
    }
}

// ---------------------------------------------------------------------------
// Control bar (bottom) — interactive: PTT, Mute, Volume
// ---------------------------------------------------------------------------

const MODES: &[(&str, Mode)] = &[
    ("LSB", Mode::LSB),
    ("USB", Mode::USB),
    ("CW", Mode::CW),
    ("CWR", Mode::CWR),
    ("AM", Mode::AM),
    ("FM", Mode::FM),
    ("DRM", Mode::DRM),
];

const STEPS: &[(&str, u64)] = &[
    ("100 Hz", 100),
    ("1 kHz", 1_000),
    ("5 kHz", 5_000),
    ("10 kHz", 10_000),
    ("25 kHz", 25_000),
];

#[derive(Clone)]
pub struct ControlBar {
    container: GtkBox,
    /// Sole source selector. Untoggled = AUD (radio USB audio), toggled
    /// = IQ (software demod). Auto-forced and hidden when only one
    /// source is available.
    audio_btn: ToggleButton,
    ptt_btn: ToggleButton,
    agc_label: Label,
    agc_scale: Scale,
    /// Tune controls (freq entry, step, up/down). Visible when CAT is
    /// available.
    sdr_box: GtkBox,
    mode_dropdown: DropDown,
    mode_list: StringList,
    /// DRM spectrum-flip toggle — visible only when current demod mode
    /// is DRM. Mirrors the server's `flip_spectrum` watch.
    flip_btn: ToggleButton,
    /// Set to true while `apply_capabilities` syncs the flip toggle
    /// from caps so the transient `set_active` doesn't round-trip back
    /// to the server.
    suppress_flip_notify: Rc<Cell<bool>>,
    /// Modes currently offered in the dropdown — filtered to server capabilities.
    active_modes: Rc<RefCell<Vec<(&'static str, Mode)>>>,
    /// Set to true while `apply_capabilities` rewires the mode list so its
    /// transient `set_selected` calls don't fire the user-intent handler.
    suppress_mode_notify: Rc<Cell<bool>>,
    /// WS sender — retained so `apply_capabilities` can send the initial
    /// AGC-threshold command after the server advertises `has_hardware_cat`.
    ws_tx: mpsc::UnboundedSender<ClientMsg>,
    /// Last-known radio state from CAT polling.
    last_radio: Rc<RefCell<Option<RadioState>>>,
    /// Persisted SDR operating parameters.
    sdr_params: Rc<RefCell<SdrParams>>,
    /// Timestamp of last user command — suppress sync briefly after.
    /// Cloned into the per-control closures that mutate it; held on the
    /// struct so a clone of `ControlBar` keeps the same shared state.
    #[allow(dead_code)]
    last_cmd: Rc<Cell<Instant>>,
    /// Display bar handle — needed by `apply_capabilities` so the
    /// AUD/IQ availability indicators can be repainted when server
    /// capabilities arrive.
    display_bar: DisplayBar,
    /// DSP toggles laid out per `docs/client-sdr-UI.drawio` IQ-NO-DRM
    /// layer. `nb`/`apf` live in `ctrl1-left`, `dnr`/`dnf` in
    /// `ctrl2-left`. The audio-domain `DNB` stage has no UI button
    /// per the diagram — it's still reachable via the persisted
    /// snapshot (and the server pipeline honours the flag) but
    /// hidden from the normal operator flow.
    nb_btn: ToggleButton,
    apf_btn: ToggleButton,
    dnr_btn: ToggleButton,
    dnf_btn: ToggleButton,
    /// REC toggle + status line (ctrl1-right per drawio). Button
    /// state tracks the server's authoritative `RecordingStatus`.
    rec_btn: ToggleButton,
    rec_status_label: Label,
    /// IF-demod mode buttons (ctrl1-center). One `ToggleButton` per
    /// mode acts as a radio group — exactly one stays pressed and
    /// drives `ClientMsg::SetDemodMode` + the matching CAT command.
    mode_btns: Vec<(Mode, ToggleButton)>,
    /// Audio-domain decoder toggles (ctrl2-center). Each is an
    /// independent `SetDecoder` — multiple can run in parallel.
    /// All are non-functional on the server side today (no Tier-3
    /// decoders wired up) but present in the UI per the drawio.
    decoder_btns: Vec<(efd_proto::DecoderKind, ToggleButton)>,
    /// Set while `apply_rec_status` / `apply_snapshot` /
    /// `sync_from_radio` propagate server state into the UI, so the
    /// `connect_toggled` handlers don't bounce the value back as a
    /// fresh `ClientMsg`.
    suppress_toggle_notify: Rc<Cell<bool>>,
}

impl ControlBar {
    pub fn new(
        ws_tx: mpsc::UnboundedSender<ClientMsg>,
        audio: Option<Arc<AudioPlayer>>,
        display_bar: DisplayBar,
    ) -> Self {
        // Three-row layout: ctrl0 has left/center/right slots for the
        // MODE button + always-visible controls; ctrl1's center slot
        // hosts the SDR-only controls (visibility-toggled by MODE);
        // ctrl2 is reserved for future controls. All three rows get a
        // fixed minimum height so the control bar doesn't collapse as
        // sdr_box / audio_btn toggle visibility.
        const CTRL_ROW_HEIGHT: i32 = 42;
        let container = GtkBox::new(Orientation::Vertical, 2);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);
        container.set_hexpand(true);

        let (ctrl0_row, ctrl0_left, ctrl0_center, ctrl0_right) = make_lcr_row();
        ctrl0_row.set_size_request(-1, CTRL_ROW_HEIGHT);
        container.append(&ctrl0_row);
        let (ctrl1_row, ctrl1_left, ctrl1_center, ctrl1_right) = make_lcr_row();
        ctrl1_row.set_size_request(-1, CTRL_ROW_HEIGHT);
        container.append(&ctrl1_row);
        let (ctrl2_row, ctrl2_left, ctrl2_center, ctrl2_right) = make_lcr_row();
        ctrl2_row.set_size_request(-1, CTRL_ROW_HEIGHT);
        container.append(&ctrl2_row);

        let last_cmd = Rc::new(Cell::new(Instant::now() - std::time::Duration::from_secs(10)));
        let last_radio: Rc<RefCell<Option<RadioState>>> = Rc::new(RefCell::new(None));
        let sdr_params = Rc::new(RefCell::new(sdr_params::load()));

        // --- Tune controls box (freq entry + step + tune up/down) ---
        // Visible whenever CAT is available; the mode dropdown lives
        // outside this box because it's meaningful even in AUD mode
        // (where it commands the radio via CAT rather than the backend).
        let sdr_box = GtkBox::new(Orientation::Horizontal, 8);

        let freq_entry = Entry::new();
        freq_entry.set_width_chars(14);
        freq_entry.set_placeholder_text(Some("Freq Hz"));
        freq_entry.add_css_class("monospace");
        {
            let tx = ws_tx.clone();
            let lc = last_cmd.clone();
            let db = display_bar.clone();
            let sp = sdr_params.clone();
            freq_entry.connect_activate(move |entry| {
                let text = entry.text();
                if let Ok(hz) = text.replace(['.', ',', ' '], "").parse::<u64>() {
                    lc.set(Instant::now());
                    db.set_freq_immediate(hz);
                    let _ = tx.send(ClientMsg::CatCommand(cat_commands::set_freq(hz)));
                    sp.borrow_mut().freq_hz = hz;
                }
            });
        }
        sdr_box.append(&freq_entry);

        // Demod-mode dropdown — lives outside `sdr_box` so it stays
        // visible in both AUD and IQ source modes.
        //   AUD + CAT: sends CAT to the radio ("tune to this mode")
        //   AUD + no-CAT (portable): greyed out
        //   IQ  + ...: sends SetDemodMode to the backend (+ CAT if any)
        // Dropdown is populated from `active_modes`, which defaults to
        // all MODES and is re-filtered when server `Capabilities` arrive.
        let active_modes: Rc<RefCell<Vec<(&'static str, Mode)>>> =
            Rc::new(RefCell::new(MODES.to_vec()));
        let suppress_mode_notify = Rc::new(Cell::new(false));
        let suppress_flip_notify = Rc::new(Cell::new(false));
        let mode_list = StringList::new(&MODES.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let mode_dropdown = DropDown::new(Some(mode_list.clone()), gtk4::Expression::NONE);
        mode_dropdown.set_selected(1); // default USB
        mode_dropdown.set_valign(Align::Center);
        // DRM spectrum-flip toggle; visible only when mode=DRM.
        let flip_btn = ToggleButton::with_label("Flip");
        flip_btn.set_valign(Align::Center);
        flip_btn.set_visible(false);
        flip_btn.set_tooltip_text(Some(
            "DRM spectrum flip — toggle when DREAM can't lock onto a broadcast",
        ));
        {
            let tx = ws_tx.clone();
            let suppress = suppress_flip_notify.clone();
            flip_btn.connect_toggled(move |btn| {
                if suppress.get() {
                    return;
                }
                let _ = tx.send(ClientMsg::SetDrmFlipSpectrum(btn.is_active()));
            });
        }

        {
            let tx = ws_tx.clone();
            let sp = sdr_params.clone();
            let am = active_modes.clone();
            let suppress = suppress_mode_notify.clone();
            let db = display_bar.clone();
            let fb = flip_btn.clone();
            mode_dropdown.connect_selected_notify(move |dd| {
                if suppress.get() {
                    return;
                }
                let idx = dd.selected() as usize;
                if let Some(&(_, mode)) = am.borrow().get(idx) {
                    let _ = tx.send(ClientMsg::SetDemodMode(Some(mode)));
                    if let Some(cmd) = cat_commands::set_mode(mode) {
                        let _ = tx.send(ClientMsg::CatCommand(cmd));
                    }
                    sp.borrow_mut().set_mode(mode);
                    // Repurpose the two extra rows for the new mode.
                    if mode == Mode::DRM {
                        db.prime_drm_placeholders();
                    } else {
                        db.clear_extras();
                    }
                    fb.set_visible(mode == Mode::DRM);
                }
            });
        }
        // --- Source toggle (sole mode selector) ---
        // Untoggled = AUD  (radio's USB audio; radio does the demod)
        // Toggled   = IQ   (software demod runs on the backend's IQ feed)
        let audio_btn = ToggleButton::with_label("SRC");
        audio_btn.set_valign(Align::Center);
        audio_btn.set_tooltip_text(Some(
            "Audio source — untoggled: radio USB audio (AUD); toggled: software demod (IQ)",
        ));
        {
            let tx = ws_tx.clone();
            let lr = last_radio.clone();
            let sp = sdr_params.clone();
            let fe = freq_entry.clone();
            let md = mode_dropdown.clone();
            let am = active_modes.clone();
            let db = display_bar.clone();
            audio_btn.connect_toggled(move |btn| {
                let is_iq = btn.is_active();
                db.set_selected_source(is_iq);
                if is_iq {
                    // --- AUD → IQ ---
                    let (freq_hz, mode) = {
                        let params = sp.borrow();
                        (params.freq_hz, params.mode())
                    };
                    let _ = tx.send(ClientMsg::CatCommand(cat_commands::set_freq(freq_hz)));
                    if let Some(cmd) = cat_commands::set_mode(mode) {
                        let _ = tx.send(ClientMsg::CatCommand(cmd));
                    }
                    let _ = tx.send(ClientMsg::SetDemodMode(Some(mode)));
                    let _ = tx.send(ClientMsg::SelectSource(SourceClass::Iq));

                    fe.set_text(&format!("{}", freq_hz));
                    if let Some(idx) = am.borrow().iter().position(|&(_, m)| m == mode) {
                        md.set_selected(idx as u32);
                    }
                    if mode == Mode::DRM {
                        db.prime_drm_placeholders();
                    } else {
                        db.clear_extras();
                    }
                } else {
                    // --- IQ → AUD ---
                    db.clear_extras();
                    {
                        let mut params = sp.borrow_mut();
                        if let Some(ref state) = *lr.borrow() {
                            params.freq_hz = state.freq_hz;
                            params.set_mode(state.mode);
                        }
                        sdr_params::save(&params);
                    }
                    let _ = tx.send(ClientMsg::SetDemodMode(None));
                    let _ = tx.send(ClientMsg::SelectSource(SourceClass::Audio));
                }
            });
        }

        ctrl0_left.append(&audio_btn);

        // --- AGC threshold slider (always visible, 0–10) ---
        let initial_threshold = sdr_params.borrow().agc_threshold;
        let agc_label = Label::new(Some("AGC:"));
        agc_label.add_css_class("monospace");
        agc_label.set_valign(Align::Center);
        ctrl0_center.append(&agc_label);

        let agc_adj = Adjustment::new(initial_threshold as f64, 0.0, 10.0, 1.0, 1.0, 0.0);
        let agc_scale = Scale::new(Orientation::Horizontal, Some(&agc_adj));
        agc_scale.set_width_request(140);
        agc_scale.set_valign(Align::Center);
        agc_scale.set_draw_value(true);
        agc_scale.set_digits(0);
        agc_scale.set_round_digits(0);
        for i in 0..=10 {
            agc_scale.add_mark(i as f64, gtk4::PositionType::Bottom, None);
        }
        {
            let tx = ws_tx.clone();
            let sp = sdr_params.clone();
            let lc = last_cmd.clone();
            agc_adj.connect_value_changed(move |adj| {
                let v = adj.value().round() as u8;
                lc.set(Instant::now());
                sp.borrow_mut().agc_threshold = v;
                let _ = tx.send(ClientMsg::CatCommand(cat_commands::set_agc_threshold(v)));
            });
        }
        ctrl0_center.append(&agc_scale);

        // No unconditional `SelectSource` on init — the server seeds
        // its own default (via StateSnapshot / AudioRouting) and a
        // blind "Audio" send here caused a noisy fallback loop when
        // USB audio wasn't available (RadioUsb → fallback to
        // SoftwareDemod → two log lines per connect). The client's
        // source toggle fires `SelectSource` on user click.

        // Step size dropdown
        let step_list = StringList::new(&STEPS.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let step_dropdown = DropDown::new(Some(step_list), gtk4::Expression::NONE);
        step_dropdown.set_selected(1); // default 1 kHz
        step_dropdown.set_valign(Align::Center);
        sdr_box.append(&step_dropdown);

        // Tune up/down
        {
            let tune_down = Button::with_label("\u{25BC}"); // ▼
            tune_down.set_valign(Align::Center);
            let fe = freq_entry.clone();
            let sd = step_dropdown.clone();
            let tx = ws_tx.clone();
            let lc = last_cmd.clone();
            let db = display_bar.clone();
            let sp = sdr_params.clone();
            tune_down.connect_clicked(move |_| {
                tune_by_step(&fe, &sd, &tx, &db, &lc, &sp, false);
            });
            sdr_box.append(&tune_down);
        }
        {
            let tune_up = Button::with_label("\u{25B2}"); // ▲
            tune_up.set_valign(Align::Center);
            let fe = freq_entry.clone();
            let sd = step_dropdown.clone();
            let tx = ws_tx.clone();
            let lc = last_cmd.clone();
            let db = display_bar.clone();
            let sp = sdr_params.clone();
            tune_up.connect_clicked(move |_| {
                tune_by_step(&fe, &sd, &tx, &db, &lc, &sp, true);
            });
            sdr_box.append(&tune_up);
        }

        // Per the drawio, ctrl1-center is reserved for the IF-demod
        // mode buttons (AM/SAM/DSB/USB/LSB/CWᵤ/CWₗ/FMₙ). The tune
        // controls + flip toggle move to ctrl0-center alongside the
        // other always-visible controls, since the diagram uses
        // ctrl0 for tuning and ctrl1/ctrl2 for mode+decoder pickers.
        ctrl0_center.append(&sdr_box);
        ctrl0_center.append(&flip_btn);
        // mode_dropdown kept but hidden — it still sources CSS/id for
        // some existing tests. Mode buttons below drive the real
        // selection.
        mode_dropdown.set_visible(false);
        ctrl0_center.append(&mode_dropdown);

        // --- Always-visible controls: PTT, Mute, Volume ---
        let ptt_btn = ToggleButton::with_label("PTT");
        ptt_btn.set_valign(Align::Center);
        let tx = ws_tx.clone();
        ptt_btn.connect_toggled(move |btn| {
            let on = btn.is_active();
            let _ = tx.send(ClientMsg::Ptt(Ptt { on }));
        });
        ctrl0_center.append(&ptt_btn);

        if let Some(ref player) = audio {
            let mute_btn = ToggleButton::with_label("Mute");
            mute_btn.set_valign(Align::Center);
            let ap = player.clone();
            mute_btn.connect_toggled(move |btn| {
                let muted = btn.is_active();
                if muted != ap.is_muted() {
                    ap.toggle_mute();
                }
            });
            ctrl0_center.append(&mute_btn);

            let vol_label = Label::new(Some("Vol:"));
            vol_label.add_css_class("monospace");
            ctrl0_center.append(&vol_label);

            let vol_adj = Adjustment::new(70.0, 0.0, 100.0, 5.0, 10.0, 0.0);
            let vol_scale = Scale::new(Orientation::Horizontal, Some(&vol_adj));
            vol_scale.set_width_request(100);
            vol_scale.set_valign(Align::Center);
            vol_scale.set_draw_value(false);
            let ap = player.clone();
            vol_adj.connect_value_changed(move |adj| {
                ap.set_volume(adj.value() as f32 / 100.0);
            });
            ctrl0_center.append(&vol_scale);
        }

        // -----------------------------------------------------------
        // DSP toggles (NB / APF / DNR / DNF) + REC + CONFIG + WSJT-X,
        // placed per the drawio IQ-NO-DRM layer:
        //   ctrl1-left:  NB, APF
        //   ctrl1-right: REC
        //   ctrl2-left:  DNR, DNF
        //   ctrl2-right: CONFIG
        //   ctrl0-right: WSJT-X
        // The audio-domain `DNB` stage is *not* surfaced as a button
        // per the diagram; it remains reachable via the persisted
        // snapshot and the pipeline honours the flag, but normal
        // operator flow uses the pre-IF NB only.
        let suppress_toggle_notify = Rc::new(Cell::new(false));

        fn make_dsp_toggle(
            label: &str,
            tooltip: &str,
            ws: &mpsc::UnboundedSender<ClientMsg>,
            suppress: &Rc<Cell<bool>>,
            mk_msg: impl Fn(bool) -> ClientMsg + 'static,
        ) -> ToggleButton {
            let btn = ToggleButton::with_label(label);
            btn.set_valign(Align::Center);
            btn.set_tooltip_text(Some(tooltip));
            btn.add_css_class("dsp-toggle");
            let ws = ws.clone();
            let suppress = suppress.clone();
            btn.connect_toggled(move |b| {
                if suppress.get() {
                    return;
                }
                let _ = ws.send(mk_msg(b.is_active()));
            });
            btn
        }

        let nb_btn = make_dsp_toggle(
            "NB",
            "Pre-IF noise blanker (envelope-threshold impulse blanker on raw IQ)",
            &ws_tx,
            &suppress_toggle_notify,
            ClientMsg::SetNb,
        );
        let apf_btn = make_dsp_toggle(
            "APF",
            "Audio Peak Filter — phase 3c, currently pass-through",
            &ws_tx,
            &suppress_toggle_notify,
            ClientMsg::SetApf,
        );
        let dnr_btn = make_dsp_toggle(
            "DNR",
            "Digital Noise Reduction (spectral subtraction) — phase 3c, currently pass-through",
            &ws_tx,
            &suppress_toggle_notify,
            ClientMsg::SetDnr,
        );
        let dnf_btn = make_dsp_toggle(
            "DNF",
            "Digital Notch Filter — phase 3c, currently pass-through",
            &ws_tx,
            &suppress_toggle_notify,
            ClientMsg::SetDnf,
        );
        ctrl1_left.append(&nb_btn);
        ctrl1_left.append(&apf_btn);
        ctrl2_left.append(&dnr_btn);
        ctrl2_left.append(&dnf_btn);

        // --- REC toggle + status (ctrl1-right) ---
        let rec_btn = ToggleButton::with_label("REC");
        rec_btn.set_valign(Align::Center);
        rec_btn.add_css_class("chrome-btn");
        rec_btn.set_tooltip_text(Some(
            "Start/stop recording the audio stream to a file under ~/.local/state/efd-backend/recordings",
        ));
        {
            let ws = ws_tx.clone();
            let suppress = suppress_toggle_notify.clone();
            rec_btn.connect_toggled(move |b| {
                if suppress.get() {
                    return;
                }
                let msg = if b.is_active() {
                    ClientMsg::StartRecording(StartRecording {
                        kind: RecKind::Audio,
                        path: None,
                    })
                } else {
                    ClientMsg::StopRecording
                };
                let _ = ws.send(msg);
            });
        }
        let rec_status_label = Label::new(Some(""));
        rec_status_label.add_css_class("monospace");
        rec_status_label.set_xalign(1.0);
        rec_status_label.set_halign(Align::End);
        ctrl1_right.append(&rec_btn);
        ctrl1_right.append(&rec_status_label);

        // --- WSJT-X launcher (ctrl0-right) ---
        // Phase-5b placeholder: click logs a TODO. Real launcher is
        // a follow-up — it needs to know how to spawn WSJT-X and
        // point it at the rigctld tunnel we document in README.
        let wsjtx_btn = Button::with_label("WSJT-X");
        wsjtx_btn.set_valign(Align::Center);
        wsjtx_btn.add_css_class("chrome-btn");
        wsjtx_btn.set_tooltip_text(Some(
            "Launch WSJT-X (phase-5c placeholder — no-op until the launcher ships)",
        ));
        wsjtx_btn.connect_clicked(|_| {
            eprintln!("[wsjtx] launcher not yet implemented (phase 5c)");
        });
        ctrl0_right.append(&wsjtx_btn);

        // --- CONFIG dialog (ctrl2-right) ---
        // Phase-5b placeholder: click logs a TODO. The dialog will
        // eventually expose server URL, token, recording dir,
        // start-up DSP defaults.
        let config_btn = Button::with_label("CONFIG");
        config_btn.set_valign(Align::Center);
        config_btn.add_css_class("chrome-btn");
        config_btn.set_tooltip_text(Some(
            "Open client settings (phase-5c placeholder)",
        ));
        config_btn.connect_clicked(|_| {
            eprintln!("[config] dialog not yet implemented (phase 5c)");
        });
        ctrl2_right.append(&config_btn);

        // -----------------------------------------------------------
        // ctrl1-center — IF-demod mode buttons.
        // -----------------------------------------------------------
        // Row of linked `ToggleButton`s acting as a radio group:
        // exactly one stays pressed, and pressing any other clears
        // the previous one. Replaces the mode_dropdown per the
        // drawio. Each click sends `SetDemodMode` (backend software
        // demod) plus the matching CAT command (when hardware CAT
        // is available). Filter-by-capabilities happens in
        // `apply_capabilities`.
        let mode_btns = build_mode_buttons(
            &ctrl1_center,
            &ws_tx,
            &sdr_params,
            &display_bar,
            &flip_btn,
            &suppress_toggle_notify,
        );

        // -----------------------------------------------------------
        // ctrl2-center — audio-domain decoder toggles.
        // -----------------------------------------------------------
        // Per the drawio: CW / PSK / MFSK / RTTY / FAX / PCKT
        // (audio decoders, purple) | DRM / FDV (DRM / FreeDV, pink).
        // Each is an independent `SetDecoder` — multiple can run at
        // once. Server-side Tier-3 decoders aren't wired up yet, so
        // these are click-only visible today; the proto contract is
        // honoured so they'll light up as soon as the backend
        // implementations land.
        let decoder_btns =
            build_decoder_buttons(&ctrl2_center, &ws_tx, &suppress_toggle_notify);

        Self {
            container,
            audio_btn,
            ptt_btn,
            agc_label,
            agc_scale,
            sdr_box,
            mode_dropdown,
            mode_list,
            flip_btn,
            suppress_flip_notify,
            active_modes,
            suppress_mode_notify,
            ws_tx,
            last_radio,
            sdr_params,
            last_cmd,
            display_bar,
            nb_btn,
            apf_btn,
            dnr_btn,
            dnf_btn,
            rec_btn,
            rec_status_label,
            mode_btns,
            decoder_btns,
            suppress_toggle_notify,
        }
    }

    /// Mirror server-authoritative `RecordingStatus` into the REC
    /// toggle + status label. Called from the WS dispatch.
    pub fn apply_rec_status(&self, status: &RecordingStatus) {
        self.suppress_toggle_notify.set(true);
        self.rec_btn.set_active(status.active);
        self.suppress_toggle_notify.set(false);

        if status.active {
            let kind_str = match status.kind {
                Some(RecKind::Iq) => "IQ",
                Some(RecKind::Audio) => "audio",
                None => "?",
            };
            let duration = status
                .duration_s
                .map(|s| format!("{:>5.1}s", s))
                .unwrap_or_else(|| "  ---".to_string());
            let kib = status.bytes_written as f64 / 1024.0;
            self.rec_status_label
                .set_text(&format!("REC {kind_str}: {duration}  {kib:.0} KiB"));
        } else {
            self.rec_status_label.set_text("");
        }
    }

    /// Seed the DSP toggles and decoder state from a server-pushed
    /// `StateSnapshot` so persisted user preferences (e.g. "DNR
    /// always on") show up correctly on reconnect and after server
    /// restarts.
    pub fn apply_snapshot(&self, snap: &StateSnapshot) {
        self.suppress_toggle_notify.set(true);
        self.nb_btn.set_active(snap.nb_on);
        self.apf_btn.set_active(snap.apf_on);
        self.dnr_btn.set_active(snap.dnr_on);
        self.dnf_btn.set_active(snap.dnf_on);
        // Mode buttons: exactly one active per the snapshot's mode.
        for (m, btn) in &self.mode_btns {
            btn.set_active(*m == snap.mode);
        }
        // Decoder buttons: each active iff in enabled_decoders.
        for (kind, btn) in &self.decoder_btns {
            btn.set_active(snap.enabled_decoders.contains(kind));
        }
        self.suppress_toggle_notify.set(false);
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Sync control bar from RadioState.
    pub fn sync_from_radio(&self, state: &RadioState) {
        *self.last_radio.borrow_mut() = Some(state.clone());
    }

    /// Gate UI controls by server-advertised source capabilities.
    pub fn apply_capabilities(&self, caps: &Capabilities) {
        self.ptt_btn.set_visible(caps.has_tx);
        // AGC threshold is a CAT command, so it keys on has_hardware_cat.
        self.agc_label.set_visible(caps.has_hardware_cat);
        self.agc_scale.set_visible(caps.has_hardware_cat);
        // AUD / IQ availability indicators in the display bar.
        self.display_bar
            .set_source_availability(caps.has_usb_audio, caps.has_iq);

        // DRM flip toggle — sync initial state from the server's
        // advertised value (usually seeded from its config.toml).
        // Suppressed so the programmatic set_active doesn't round-trip
        // back to the server as a "client wants to change this" message.
        if self.flip_btn.is_active() != caps.drm_flip_spectrum {
            self.suppress_flip_notify.set(true);
            self.flip_btn.set_active(caps.drm_flip_spectrum);
            self.suppress_flip_notify.set(false);
        }

        // Source selection — the sole mode choice. Button visible only
        // when both sources are available so the user can pick; hidden
        // + auto-forced otherwise. Construction default is AUD, so the
        // both-available case needs no forcing here.
        match (caps.has_usb_audio, caps.has_iq) {
            (true, true) => self.audio_btn.set_visible(true),
            (true, false) => {
                self.audio_btn.set_visible(false);
                if self.audio_btn.is_active() {
                    self.audio_btn.set_active(false); // force AUD
                }
            }
            (false, true) => {
                self.audio_btn.set_visible(false);
                if !self.audio_btn.is_active() {
                    self.audio_btn.set_active(true); // force IQ
                }
            }
            (false, false) => self.audio_btn.set_visible(false),
        }

        // Demod-mode dropdown is greyed out only in the AUD+no-CAT
        // case (portable radio): no radio to command, no backend demod
        // to configure. Active in every other combo.
        let is_aud = !self.audio_btn.is_active();
        let dropdown_sensitive = !(is_aud && !caps.has_hardware_cat);
        self.mode_dropdown.set_sensitive(dropdown_sensitive);

        // Tune controls (freq entry, step, up/down) only work via CAT.
        self.sdr_box.set_visible(caps.has_hardware_cat);

        // Initial AGC-threshold sync, deferred from construction so we only
        // emit it to sources that can accept the CAT command.
        if caps.has_hardware_cat {
            let threshold = self.sdr_params.borrow().agc_threshold;
            let _ = self
                .ws_tx
                .send(ClientMsg::CatCommand(cat_commands::set_agc_threshold(
                    threshold,
                )));
        }

        let filtered: Vec<(&'static str, Mode)> = MODES
            .iter()
            .copied()
            .filter(|(_, m)| caps.supported_demod_modes.contains(m))
            .collect();
        if filtered.is_empty() {
            return;
        }

        let prev_mode = {
            let am = self.active_modes.borrow();
            am.get(self.mode_dropdown.selected() as usize)
                .map(|&(_, m)| m)
        };

        let new_strs: Vec<&str> = filtered.iter().map(|&(s, _)| s).collect();
        let n_current = self.mode_list.n_items();
        let new_idx = prev_mode
            .and_then(|m| filtered.iter().position(|&(_, fm)| fm == m))
            .or_else(|| filtered.iter().position(|&(_, m)| m == Mode::USB))
            .unwrap_or(0);

        // Suppress the mode-dropdown user-intent handler while we rewire the
        // model — otherwise the transient set_selected calls would issue
        // spurious SetDemodMode + MD; CAT commands on every capabilities
        // advertisement.
        self.suppress_mode_notify.set(true);
        self.mode_list.splice(0, n_current, &new_strs);
        *self.active_modes.borrow_mut() = filtered;
        self.mode_dropdown.set_selected(new_idx as u32);
        self.suppress_mode_notify.set(false);
    }

    /// Save SDR params if currently using the IQ source (call on app quit).
    pub fn save_on_quit(&self) {
        if self.audio_btn.is_active() {
            let mut params = self.sdr_params.borrow_mut();
            if let Some(ref state) = *self.last_radio.borrow() {
                params.freq_hz = state.freq_hz;
                params.set_mode(state.mode);
            }
            sdr_params::save(&params);
        }
    }
}

/// Tune up/down by step. Sends command immediately — the server coalesces
/// rapid commands so only the last frequency actually gets sent to the radio.
fn tune_by_step(
    freq_entry: &Entry,
    step_dropdown: &DropDown,
    ws_tx: &mpsc::UnboundedSender<ClientMsg>,
    display_bar: &DisplayBar,
    last_cmd: &Rc<Cell<Instant>>,
    sdr_params: &Rc<RefCell<SdrParams>>,
    up: bool,
) {
    let step_idx = step_dropdown.selected() as usize;
    let step_hz = STEPS.get(step_idx).map(|&(_, hz)| hz).unwrap_or(1000);
    let current: u64 = freq_entry
        .text()
        .replace(['.', ',', ' '], "")
        .parse()
        .unwrap_or(0);
    let new_freq = if up {
        current.saturating_add(step_hz)
    } else {
        current.saturating_sub(step_hz)
    };
    freq_entry.set_text(&format!("{new_freq}"));
    display_bar.set_freq_immediate(new_freq);
    last_cmd.set(Instant::now());
    sdr_params.borrow_mut().freq_hz = new_freq;
    let _ = ws_tx.send(ClientMsg::CatCommand(cat_commands::set_freq(new_freq)));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Toggle a label between the active (blue `.app-mode`) and inactive
/// (grey `.app-mode-disabled`) styles. Used by the AUD / IQ
/// availability indicators in the display bar.
fn apply_avail_style(label: &Label, available: bool) {
    if available {
        label.add_css_class("app-mode");
        label.remove_css_class("app-mode-disabled");
    } else {
        label.remove_css_class("app-mode");
        label.add_css_class("app-mode-disabled");
    }
}

/// Build a horizontal row with three slots: left (start-aligned, fixed
/// width), center (expanding, center-aligned), right (end-aligned, fixed
/// width). Matches the disp{0,1,2}/ctrl0 cell layout in
/// docs/client-sdr-UI.drawio.
fn make_lcr_row() -> (GtkBox, GtkBox, GtkBox, GtkBox) {
    const SIDE_WIDTH: i32 = 120;

    let row = GtkBox::new(Orientation::Horizontal, 0);
    row.set_hexpand(true);

    let left = GtkBox::new(Orientation::Horizontal, 8);
    left.set_halign(Align::Start);
    left.set_hexpand(false);
    left.set_size_request(SIDE_WIDTH, -1);
    row.append(&left);

    let center = GtkBox::new(Orientation::Horizontal, 12);
    center.set_halign(Align::Center);
    center.set_hexpand(true);
    row.append(&center);

    let right = GtkBox::new(Orientation::Horizontal, 8);
    right.set_halign(Align::End);
    right.set_hexpand(false);
    right.set_size_request(SIDE_WIDTH, -1);
    row.append(&right);

    (row, left, center, right)
}

/// Build the IF-demod mode button row for `ctrl1-center`. Each
/// button represents one `Mode` and sends `ClientMsg::SetDemodMode`
/// plus the matching CAT `MD…;` command when clicked. Behaves as a
/// radio group: pressing any button clears the previously-active
/// one, including the case where the user clicks the already-active
/// button (that flips it off → the server falls back to whatever
/// mode the RadioState reports; a follow-up will tighten that into
/// a "one stays on always" policy).
fn build_mode_buttons(
    parent: &GtkBox,
    ws_tx: &mpsc::UnboundedSender<ClientMsg>,
    sdr_params: &Rc<RefCell<SdrParams>>,
    display_bar: &DisplayBar,
    flip_btn: &ToggleButton,
    suppress: &Rc<Cell<bool>>,
) -> Vec<(Mode, ToggleButton)> {
    // Order + subscript glyphs match the drawio IQ-NO-DRM layer.
    // `Mode::CW` is CW-upper; `Mode::CWR` is CW-lower; the unicode
    // subscripts u/ₗ come out crisp in the Hack Nerd Font Mono we
    // already load.
    let spec: &[(&str, Mode)] = &[
        ("AM", Mode::AM),
        ("SAM", Mode::SAM),
        ("DSB", Mode::DSB),
        ("USB", Mode::USB),
        ("LSB", Mode::LSB),
        ("CWᵤ", Mode::CW),
        ("CWₗ", Mode::CWR),
        ("FMₙ", Mode::FM),
    ];

    let row = GtkBox::new(Orientation::Horizontal, 4);
    row.add_css_class("linked");
    let mut out: Vec<(Mode, ToggleButton)> = Vec::with_capacity(spec.len());
    // Use a shared Rc<RefCell<Vec<_>>> for inter-button awareness
    // so clicking one can un-press the others.
    let group: Rc<RefCell<Vec<(Mode, ToggleButton)>>> = Rc::new(RefCell::new(Vec::new()));

    for (label, mode) in spec {
        let btn = ToggleButton::with_label(label);
        btn.set_valign(Align::Center);
        btn.add_css_class("mode-btn");
        btn.set_tooltip_text(Some(&format!("IF demod: {label}")));
        let ws = ws_tx.clone();
        let sp = sdr_params.clone();
        let db = display_bar.clone();
        let fb = flip_btn.clone();
        let s = suppress.clone();
        let grp = group.clone();
        let m = *mode;
        btn.connect_toggled(move |b| {
            if s.get() {
                return;
            }
            if !b.is_active() {
                // User clicked an already-active button off; re-toggle
                // it on and do nothing else — the radio group always
                // has exactly one mode active.
                s.set(true);
                b.set_active(true);
                s.set(false);
                return;
            }
            // Un-press every other button in the group.
            s.set(true);
            for (om, ob) in grp.borrow().iter() {
                if *om != m {
                    ob.set_active(false);
                }
            }
            s.set(false);
            let _ = ws.send(ClientMsg::SetDemodMode(Some(m)));
            if let Some(cmd) = cat_commands::set_mode(m) {
                let _ = ws.send(ClientMsg::CatCommand(cmd));
            }
            sp.borrow_mut().set_mode(m);
            // Show the DRM spectrum-flip toggle only when DRM is
            // selected (we don't have a dedicated DRM button in this
            // row per the diagram — DRM lives in the decoder row —
            // but the existing flip handling still applies if a
            // future phase surfaces DRM as an IF mode too).
            if m == Mode::DRM {
                db.prime_drm_placeholders();
                fb.set_visible(true);
            } else {
                db.clear_extras();
                fb.set_visible(false);
            }
        });
        row.append(&btn);
        out.push((*mode, btn));
    }
    *group.borrow_mut() = out.clone();
    parent.append(&row);
    out
}

/// Build the audio-domain decoder toggles for `ctrl2-center`. Each
/// button sends `ClientMsg::SetDecoder { decoder, enabled }` so
/// multiple decoders can run in parallel (per phase-1 proto).
/// Server-side decoders aren't wired up today — the buttons exist
/// for layout fidelity with the drawio and to exercise the
/// proto path; they'll light up for real once the backend lands
/// its Tier-3 decoders.
fn build_decoder_buttons(
    parent: &GtkBox,
    ws_tx: &mpsc::UnboundedSender<ClientMsg>,
    suppress: &Rc<Cell<bool>>,
) -> Vec<(efd_proto::DecoderKind, ToggleButton)> {
    use efd_proto::DecoderKind as D;

    // Audio-domain decoders (purple in the drawio) + DRM/FDV
    // (pink, different palette for digital voice).
    let audio_spec: &[(&str, D)] = &[
        ("CW", D::Cw),
        ("PSK", D::Psk),
        ("MFSK", D::Mfsk),
        ("RTTY", D::Rtty),
        ("FAX", D::Fax),
        ("PCKT", D::Pckt),
    ];
    let dv_spec: &[(&str, D)] = &[("DRM", D::Ft8), ("FDV", D::Aprs)];
    //                                   ^^^^^^^^^^^^^^^^^^^^^^
    // `DRM` and `FDV` in the drawio are selectors for DRM/FreeDV
    // decoding paths (Tier-2 codecs per the pipeline), which we
    // don't yet have dedicated `DecoderKind` variants for. We park
    // them on the closest available kinds (`Ft8` / `Aprs`) so the
    // buttons still emit a `SetDecoder` that distinguishes them on
    // the wire; when the proto gains `Drm` / `FreeDv` variants
    // this mapping swaps in place.

    let row = GtkBox::new(Orientation::Horizontal, 4);
    row.add_css_class("linked");
    let mut out: Vec<(D, ToggleButton)> = Vec::new();

    for (label, kind) in audio_spec.iter().chain(dv_spec.iter()) {
        let btn = ToggleButton::with_label(label);
        btn.set_valign(Align::Center);
        let css_class = if dv_spec.iter().any(|(_, k)| k == kind) {
            "decoder-drm"
        } else {
            "decoder-audio"
        };
        btn.add_css_class(css_class);
        btn.set_tooltip_text(Some(&format!(
            "{label} decoder (phase-5b placeholder — server-side decoders land later)"
        )));
        let ws = ws_tx.clone();
        let s = suppress.clone();
        let k = *kind;
        btn.connect_toggled(move |b| {
            if s.get() {
                return;
            }
            let _ = ws.send(ClientMsg::SetDecoder {
                decoder: k,
                enabled: b.is_active(),
            });
        });
        row.append(&btn);
        out.push((*kind, btn));
    }
    parent.append(&row);
    out
}

fn format_freq(hz: u64) -> String {
    if hz >= 1_000_000 {
        let mhz = hz / 1_000_000;
        let khz = (hz % 1_000_000) / 1_000;
        let remainder = hz % 1_000;
        format!("{mhz}.{khz:03}.{remainder:03} Hz")
    } else if hz >= 1_000 {
        let khz = hz / 1_000;
        let remainder = hz % 1_000;
        format!("{khz}.{remainder:03} Hz")
    } else {
        format!("{hz} Hz")
    }
}

fn db_to_s_reading(db: f32) -> f32 {
    if db <= -127.0 {
        0.0
    } else if db <= -73.0 {
        ((db + 127.0) / 54.0) * 15.0
    } else {
        15.0 + ((db + 73.0) / 60.0) * 15.0
    }
    .clamp(0.0, 30.0)
}

fn s_reading_to_string(reading: f32) -> String {
    if reading <= 15.0 {
        let s = (reading / 15.0 * 9.0).round() as u8;
        format!("S{s}")
    } else {
        let over = ((reading - 15.0) / 15.0 * 60.0).round() as u8;
        format!("S9+{over}")
    }
}
