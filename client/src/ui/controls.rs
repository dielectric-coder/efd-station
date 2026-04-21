use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use efd_proto::{
    Capabilities, ClientMsg, ControlTarget, DeviceId, DeviceList, DrmStatus, Mode, Ptt, RadioState,
    RecKind, RecordingStatus, SourceClass, StartRecording, StateSnapshot,
};
use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Box as GtkBox, Button, DropDown, Entry, Label, LevelBar, Orientation, Scale,
    StringList, ToggleButton, Widget, Window,
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
    /// Unified tuning line (`disp0-center`) — single Pango-markup
    /// label rendering `f <freq> Hz  demod <mode>  bw <w>  RIT <r>
    /// IF <i>` from the latest `RadioState`. Replaces the
    /// freq/mode/VFO/BW/S-meter cluster that used to live here;
    /// S-meter moved to `disp1-right` per the drawio.
    tuning_line: Label,
    /// RX/TX pill (`disp0-right`).
    tx_label: Label,
    /// dBm readout next to the RX/TX pill (`disp0-right`).
    dbm_label: Label,
    /// Source-class chips (`disp0-left`, AUD + IQ). Exactly one is
    /// `.chip-active`; the other is `.chip-inactive` (available) or
    /// `.chip-disabled` (unavailable per server Capabilities).
    aud_chip: Label,
    iq_chip: Label,
    /// Selected-device labels (`disp1-left`) — one under each class
    /// chip. Show the short form of the currently-picked device for
    /// that class ("FDM" / "AUD0" / "—"). Updated from
    /// `ServerMsg::DeviceList.active` per class.
    selected_aud_label: Label,
    selected_iq_label: Label,
    /// Active-source pill (`disp2-left`, e.g. `FDM IQ` green). Shows
    /// `<device> <class>` for the current live source.
    active_source_pill: Label,
    /// Audio-routing indicator (`disp2-right`, e.g. `PASSTHROUGH`).
    passthrough_pill: Label,
    /// S-meter bar + numeric label (`disp1-right` per the drawio).
    smeter: LevelBar,
    smeter_label: Label,
    /// First DRM info line — mode/bandwidth/modulation/services.
    /// Lives in `disp1-center` when in DRM mode.
    drm_line1: Label,
    /// Non-DRM status line (`disp1-center`) — `SNR … · DNR off
    /// DNF off APF off · decode …`. Mutually exclusive with
    /// `drm_line1`: one's visible while the other's hidden, driven
    /// by the current mode.
    status_line: Label,
    /// Cached DSP-flag + decoder state written by `apply_snapshot`
    /// and read by `refresh_status_line` — lets us compose the
    /// status line whenever either `RadioState` or the snapshot
    /// changes.
    cached_dsp: Rc<RefCell<StatusLineCache>>,
    /// Second DRM info line — SNR/WMER/lock flags.
    drm_line2: Label,
    /// Scrolling decoded-text output (CW / RTTY / PSK / …), shown in
    /// `disp2-center`. Each incoming `DecodedText` message appends a
    /// line, keeping at most `DECODED_LINES_KEPT` so the widget
    /// doesn't grow unbounded.
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

/// Fields `refresh_status_line` needs that don't live on the
/// incoming `RadioState` — the DSP flags and enabled decoders are
/// snapshot-driven, the SNR comes from the radio state.
#[derive(Clone, Default)]
struct StatusLineCache {
    snr_db: Option<f32>,
    nb_on: bool,
    dnr_on: bool,
    dnf_on: bool,
    apf_on: bool,
    decoders: Vec<efd_proto::DecoderKind>,
    in_drm: bool,
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
        let (row1, disp1_left, disp1_center, disp1_right) = make_lcr_row();
        container.append(&row1);
        let (row2, disp2_left, disp2_center, disp2_right) = make_lcr_row();
        container.append(&row2);

        // --- disp0-left: source-class indicator chips (AUD + IQ) ---
        // Non-interactive: one stays `.chip-active`, the other
        // `.chip-inactive`, painted by `set_device_list` from the
        // server's `DeviceList.active.kind.class()`. Picker dialogs
        // live on the AUDdev / IQdev buttons in the control bar.
        let aud_chip = make_chip("AUD");
        let iq_chip = make_chip("IQ");
        paint_chip(&aud_chip, ChipState::Active);
        paint_chip(&iq_chip, ChipState::Inactive);
        disp0_left.append(&aud_chip);
        disp0_left.append(&iq_chip);

        // --- disp0-center: unified tuning line. ---
        let tuning_line = Label::new(None);
        tuning_line.add_css_class("monospace");
        tuning_line.set_xalign(0.5);
        tuning_line.set_use_markup(true);
        tuning_line.set_markup(&fallback_tuning_markup());
        disp0_center.append(&tuning_line);

        // --- disp0-right: RX/TX pill + dBm. ---
        let tx_label = Label::new(Some("RX"));
        tx_label.add_css_class("monospace");
        tx_label.add_css_class("tx-rx-rx");
        tx_label.set_width_chars(2);
        tx_label.set_xalign(0.5);
        tx_label.set_halign(Align::End);
        disp0_right.append(&tx_label);

        let dbm_label = Label::new(Some("---"));
        dbm_label.add_css_class("monospace");
        dbm_label.set_xalign(1.0);
        dbm_label.set_halign(Align::End);
        disp0_right.append(&dbm_label);

        // --- disp1-left: selected-device labels, one under each class
        // chip in disp0-left. Text updates from `set_device_list` when
        // a `DeviceList.active` arrives for that class. `—` until then.
        let selected_aud_label = Label::new(Some("—"));
        selected_aud_label.add_css_class("monospace");
        selected_aud_label.set_xalign(0.5);
        let selected_iq_label = Label::new(Some("—"));
        selected_iq_label.add_css_class("monospace");
        selected_iq_label.set_xalign(0.5);
        disp1_left.append(&selected_aud_label);
        disp1_left.append(&selected_iq_label);

        // --- disp1-center: DRM info (drm_line1) XOR non-DRM status
        // (status_line). Only one is visible at a time; the current
        // mode drives which.
        let drm_line1 = Label::new(None);
        drm_line1.add_css_class("monospace");
        drm_line1.set_xalign(0.5);
        drm_line1.set_visible(false);
        disp1_center.append(&drm_line1);

        let status_line = Label::new(None);
        status_line.add_css_class("monospace");
        status_line.set_xalign(0.5);
        status_line.set_use_markup(true);
        disp1_center.append(&status_line);

        let cached_dsp = Rc::new(RefCell::new(StatusLineCache::default()));

        // --- disp1-right: S-meter + numeric readout. ---
        let smeter = LevelBar::new();
        smeter.set_min_value(0.0);
        smeter.set_max_value(30.0);
        smeter.set_value(0.0);
        smeter.set_width_request(110);
        smeter.set_height_request(10);
        smeter.set_valign(Align::Center);
        disp1_right.append(&smeter);

        let smeter_label = Label::new(Some("S0"));
        smeter_label.add_css_class("monospace");
        smeter_label.set_width_chars(5);
        smeter_label.set_xalign(1.0);
        disp1_right.append(&smeter_label);

        // --- disp2-left: active-source pill. ---
        let active_source_pill = Label::new(Some("— —"));
        active_source_pill.add_css_class("monospace");
        active_source_pill.add_css_class("chip-source");
        disp2_left.append(&active_source_pill);

        // --- disp2-center: DRM line 2 + decoded-text log. ---
        let drm_line2 = Label::new(None);
        drm_line2.add_css_class("monospace");
        drm_line2.set_xalign(0.5);
        disp2_center.append(&drm_line2);

        let decoded_text_label = Label::new(None);
        decoded_text_label.add_css_class("monospace");
        decoded_text_label.set_xalign(0.5);
        decoded_text_label.set_wrap(true);
        decoded_text_label.set_lines(DECODED_LINES_KEPT as i32);
        disp2_center.append(&decoded_text_label);
        let decoded_lines = Rc::new(RefCell::new(std::collections::VecDeque::with_capacity(
            DECODED_LINES_KEPT,
        )));

        // --- disp2-right: audio-routing indicator (PASSTHROUGH /
        // SWDEMOD). Text updated by `set_passthrough`.
        let passthrough_pill = Label::new(Some("SWDEMOD"));
        passthrough_pill.add_css_class("monospace");
        passthrough_pill.add_css_class("chip-passthrough");
        disp2_right.append(&passthrough_pill);

        Self {
            container,
            tuning_line,
            tx_label,
            dbm_label,
            aud_chip,
            iq_chip,
            selected_aud_label,
            selected_iq_label,
            active_source_pill,
            passthrough_pill,
            smeter,
            smeter_label,
            drm_line1,
            status_line,
            cached_dsp,
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

    /// Paint the AUD / IQ source-class chips in `disp0-left`. The
    /// chosen class becomes `.chip-active`, the other drops to
    /// `.chip-inactive`.
    pub fn set_selected_source(&self, is_iq: bool) {
        paint_chip(
            &self.aud_chip,
            if is_iq { ChipState::Inactive } else { ChipState::Active },
        );
        paint_chip(
            &self.iq_chip,
            if is_iq { ChipState::Active } else { ChipState::Inactive },
        );
    }

    /// Paint source-class availability onto the chip states. A
    /// class that's not available goes to `.chip-disabled`
    /// regardless of the selected-source state.
    pub fn set_source_availability(&self, has_aud: bool, has_iq: bool) {
        if !has_aud {
            paint_chip(&self.aud_chip, ChipState::Disabled);
        }
        if !has_iq {
            paint_chip(&self.iq_chip, ChipState::Disabled);
        }
    }

    /// Paint the AUD / IQ source-class chips active/inactive based on
    /// the kind's `SourceClass`. Call from `apply_capabilities` when
    /// the server advertises its active source.
    pub fn set_active_device(&self, kind: efd_proto::SourceKind) {
        let is_iq = matches!(kind.class(), efd_proto::SourceClass::Iq);
        self.set_selected_source(is_iq);
    }

    /// Update disp0-left AUD/IQ indicator chips and the disp1-left
    /// selected-device labels from `list.active`. Devices are cached
    /// on the ControlBar side (where the AUDdev / IQdev pickers live),
    /// so this method is display-only.
    pub fn set_device_list(&self, list: &efd_proto::DeviceList) {
        if let Some(active) = list.active.as_ref() {
            let is_iq = matches!(active.kind.class(), efd_proto::SourceClass::Iq);
            self.set_selected_source(is_iq);
        }

        // Update the selected-device label under whichever chip
        // matches the active device's class. The index is the
        // device's position within its class list, so audio devices
        // render as AUD0 / AUD1 / … consistently between the picker
        // and the selected label. The other class's label stays at
        // whatever it was (client-side memory of the last selection
        // for that class).
        if let Some(active) = list.active.as_ref() {
            match active.kind.class() {
                efd_proto::SourceClass::Audio => {
                    let idx = list
                        .audio_devices
                        .iter()
                        .position(|d| d == active)
                        .unwrap_or(0);
                    self.selected_aud_label
                        .set_text(&class_short_label(active, idx));
                }
                efd_proto::SourceClass::Iq => {
                    let idx = list
                        .iq_devices
                        .iter()
                        .position(|d| d == active)
                        .unwrap_or(0);
                    self.selected_iq_label
                        .set_text(&class_short_label(active, idx));
                }
            }
            // Also paint the disp2-left pill (e.g. `FDM IQ`).
            let class_tag = match active.kind.class() {
                efd_proto::SourceClass::Iq => "IQ",
                efd_proto::SourceClass::Audio => "AUD",
            };
            self.active_source_pill.set_text(&format!(
                "{} {}",
                device_short_label(active.kind),
                class_tag
            ));
        }
    }

    /// Update the disp2-left active-source pill (e.g. `FDM IQ`).
    pub fn set_active_source_label(&self, text: &str) {
        self.active_source_pill.set_text(text);
    }

    /// Update the disp2-right audio-routing indicator text.
    pub fn set_passthrough(&self, text: &str) {
        self.passthrough_pill.set_text(text);
    }

    /// Pull the DSP + decoder-intent state out of a `StateSnapshot`
    /// into the status-line cache. Called from
    /// `ControlBar::apply_snapshot`; the status line itself is
    /// redrawn by `refresh_status_line`.
    pub fn set_dsp_status(
        &self,
        nb_on: bool,
        dnr_on: bool,
        dnf_on: bool,
        apf_on: bool,
        decoders: &[efd_proto::DecoderKind],
    ) {
        {
            let mut cache = self.cached_dsp.borrow_mut();
            cache.nb_on = nb_on;
            cache.dnr_on = dnr_on;
            cache.dnf_on = dnf_on;
            cache.apf_on = apf_on;
            cache.decoders = decoders.to_vec();
        }
        self.refresh_status_line();
    }

    /// Compose the `disp1-center` status line from whichever cache
    /// fields are currently set. Pango markup makes the toggle-is-off
    /// tags dim so the operator can scan the on-state quickly.
    fn refresh_status_line(&self) {
        let cache = self.cached_dsp.borrow();
        // Swap visibility between DRM and non-DRM info for this cell.
        self.drm_line1.set_visible(cache.in_drm);
        self.status_line.set_visible(!cache.in_drm);
        if cache.in_drm {
            return;
        }

        let snr = cache
            .snr_db
            .map(|v| format!("<b>{:.0}</b> dB", v))
            .unwrap_or_else(|| "<i>—</i>".into());

        let tag = |label: &str, on: bool| -> String {
            if on {
                format!("<b>{label}</b>")
            } else {
                format!("<span alpha='45%'>{label}<sub>off</sub></span>")
            }
        };

        let decoders = if cache.decoders.is_empty() {
            "<i>—</i>".to_string()
        } else {
            cache
                .decoders
                .iter()
                .map(|d| format!("<b>{:?}</b>", d))
                .collect::<Vec<_>>()
                .join(" ")
        };

        self.status_line.set_markup(&format!(
            "SNR {snr}   {nb}  {dnr}  {dnf}  {apf}   decode {decoders}",
            nb = tag("NB", cache.nb_on),
            dnr = tag("DNR", cache.dnr_on),
            dnf = tag("DNF", cache.dnf_on),
            apf = tag("APF", cache.apf_on),
        ));
    }

    /// Optimistic frequency update (before radio confirms). Keeps
    /// the tuning line in sync with the user's typed value while
    /// we wait for the CAT poll to confirm.
    pub fn set_freq_immediate(&self, hz: u64) {
        {
            let mut prev = self.prev.borrow_mut();
            let composed = match prev.as_mut() {
                Some(s) => {
                    s.freq_hz = hz;
                    s.clone()
                }
                None => CachedState {
                    freq_hz: hz,
                    mode: "USB".into(),
                    vfo: "VFO A".into(),
                    filter_bw: String::new(),
                    s_reading: 0,
                    tx: false,
                },
            };
            *prev = Some(composed);
        }
        self.tuning_line.set_markup(&self.current_tuning_markup());
    }

    /// Build a tuning-markup string from the cached state (used by
    /// `set_freq_immediate` so we can re-render without an incoming
    /// `RadioState`).
    fn current_tuning_markup(&self) -> String {
        let prev = self.prev.borrow();
        match prev.as_ref() {
            Some(s) => {
                let freq = format_freq_spaced(s.freq_hz);
                format!(
                    "<i>f</i> <span size='x-large'><b>{freq}</b></span> Hz   \
                     demod <b>{}</b>   <i>bw</i> <b>{}</b>   <i>RIT</i> <b>0</b> Hz   <i>IF</i> <b>0</b> Hz",
                    s.mode,
                    if s.filter_bw.is_empty() { "—".into() } else { s.filter_bw.clone() },
                )
            }
            None => fallback_tuning_markup(),
        }
    }

    pub fn update(&self, state: &RadioState) {
        let s_reading = db_to_s_reading(state.s_meter_db);
        let mode_str = mode_markup(state.mode).to_string();
        let vfo_str = format!("VFO {:?}", state.vfo);

        let new_state = CachedState {
            freq_hz: state.freq_hz,
            mode: mode_str,
            vfo: vfo_str,
            filter_bw: state.filter_bw.clone(),
            s_reading: (s_reading * 10.0) as u16,
            tx: state.tx,
        };

        let mut prev = self.prev.borrow_mut();
        if prev.as_ref() == Some(&new_state) {
            return;
        }
        *prev = Some(new_state);
        drop(prev);

        // Unified tuning line (disp0-center).
        self.tuning_line.set_markup(&format_tuning_markup(state));

        // S-meter + dBm pills (disp0-right / disp1-right).
        self.smeter.set_value(s_reading as f64);
        self.smeter_label.set_text(&s_reading_to_string(s_reading));
        self.dbm_label
            .set_text(&format!("{:.0} dBm", state.s_meter_db));

        // Non-DRM status line (disp1-center). `drm_line1` owns the
        // cell while in DRM; otherwise the composed SNR+DSP+decoder
        // line does.
        {
            let mut cache = self.cached_dsp.borrow_mut();
            cache.snr_db = state.snr_db;
            cache.in_drm = state.mode == Mode::DRM;
        }
        self.refresh_status_line();

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

#[derive(Clone)]
pub struct ControlBar {
    container: GtkBox,
    ptt_btn: ToggleButton,
    /// AUDdev / IQdev picker buttons in ctrl0-left. Each opens a
    /// class-filtered device picker; visibility is gated on the
    /// server's advertised capabilities (has_usb_audio / has_iq).
    aud_dev_btn: Button,
    iq_dev_btn: Button,
    /// Cache of the latest server-pushed device list, split by class
    /// for the AUDdev / IQdev pickers. Written by `set_device_list`.
    cached_audio_devices: Rc<RefCell<Vec<DeviceId>>>,
    cached_iq_devices: Rc<RefCell<Vec<DeviceId>>>,
    /// ctrl0-center yellow chip tiles per drawio (agc / f / bw / rit
    /// / IF). Buttons are kept only for the two tiles that get
    /// visibility-gated on hardware CAT (agc + f); bw/rit/IF labels
    /// stay because `sync_from_radio` updates their markup.
    agc_tile_btn: Button,
    freq_tile_btn: Button,
    freq_tile_label: Label,
    bw_tile_label: Label,
    rit_tile_label: Label,
    if_tile_label: Label,
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
        // Shared width for the right-column buttons (WSJT-X, REC, CONFIG)
        // so they line up vertically regardless of label length.
        const CTRL_RIGHT_BTN_WIDTH: i32 = 100;
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

        // Overflow row for widgets that aren't in the drawio spec
        // (PTT, Mute, Vol, DRM flip, hidden mode dropdown). Kept here
        // so the feature set doesn't regress — move into their final
        // home (CONFIG dialog / keyboard shortcuts / etc.) later.
        let overflow_row = GtkBox::new(Orientation::Horizontal, 8);
        overflow_row.set_margin_top(4);
        overflow_row.set_halign(Align::End);
        container.append(&overflow_row);

        let last_cmd = Rc::new(Cell::new(Instant::now() - std::time::Duration::from_secs(10)));
        let last_radio: Rc<RefCell<Option<RadioState>>> = Rc::new(RefCell::new(None));
        let sdr_params = Rc::new(RefCell::new(sdr_params::load()));

        // --- Mode dropdown model ---
        // The visible mode selector is the ctrl1-center button row
        // (`build_mode_buttons`); the DropDown is kept hidden so
        // `active_modes` / `apply_capabilities` can filter MODES by
        // server capability and existing tests can still find it.
        let active_modes: Rc<RefCell<Vec<(&'static str, Mode)>>> =
            Rc::new(RefCell::new(MODES.to_vec()));
        let suppress_mode_notify = Rc::new(Cell::new(false));
        let suppress_flip_notify = Rc::new(Cell::new(false));
        let mode_list = StringList::new(&MODES.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let mode_dropdown = DropDown::new(Some(mode_list.clone()), gtk4::Expression::NONE);
        mode_dropdown.set_selected(1); // default USB
        mode_dropdown.set_visible(false);

        // DRM spectrum-flip toggle, lives in the overflow row, visible only in DRM.
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
                    if mode == Mode::DRM {
                        db.prime_drm_placeholders();
                    } else {
                        db.clear_extras();
                    }
                    fb.set_visible(mode == Mode::DRM);
                }
            });
        }

        // -----------------------------------------------------------
        // ctrl0-center — five yellow chip tiles (agc / f / bw / rit / IF).
        // Each tile shows a live Pango-markup value and opens a modal
        // edit dialog on click.
        // -----------------------------------------------------------
        const TILE_WIDTH: i32 = 100;
        let initial_threshold = sdr_params.borrow().agc_threshold;
        let (agc_tile_btn, agc_tile_label) = make_ctrl_tile(
            &format!("<i>agc {}</i>", initial_threshold),
            "AGC threshold (0–10) — click to edit",
            TILE_WIDTH,
        );
        let (freq_tile_btn, freq_tile_label) = make_ctrl_tile(
            &format!(
                "<i>f</i> {}<sup>Hz</sup>",
                format_freq_spaced(sdr_params.borrow().freq_hz)
            ),
            "Frequency (Hz) — click to edit",
            TILE_WIDTH,
        );
        let (bw_tile_btn, bw_tile_label) = make_ctrl_tile(
            "<i>bw</i> —<sup>Hz</sup>",
            "Filter bandwidth (read-only today; CAT command TBD)",
            TILE_WIDTH,
        );
        let (rit_tile_btn, rit_tile_label) = make_ctrl_tile(
            "<i>rit</i> 0<sup>Hz</sup>",
            "RIT offset (read-only today; CAT command TBD)",
            TILE_WIDTH,
        );
        let (if_tile_btn, if_tile_label) = make_ctrl_tile(
            "<i>IF</i> 0<sup>Hz</sup>",
            "IF offset (read-only today; CAT command TBD)",
            TILE_WIDTH,
        );
        ctrl0_center.append(&agc_tile_btn);
        ctrl0_center.append(&freq_tile_btn);
        ctrl0_center.append(&bw_tile_btn);
        ctrl0_center.append(&rit_tile_btn);
        ctrl0_center.append(&if_tile_btn);

        // agc tile → edit threshold (0–10), send CAT, update label.
        {
            let ws = ws_tx.clone();
            let sp = sdr_params.clone();
            let lc = last_cmd.clone();
            let lbl = agc_tile_label.clone();
            agc_tile_btn.connect_clicked(move |btn| {
                let current = sp.borrow().agc_threshold;
                let ws = ws.clone();
                let sp = sp.clone();
                let lc = lc.clone();
                let lbl = lbl.clone();
                open_tile_entry(btn, "AGC threshold", &current.to_string(), move |txt| {
                    if let Ok(v) = txt.trim().parse::<u8>() {
                        let v = v.min(10);
                        lc.set(Instant::now());
                        sp.borrow_mut().agc_threshold = v;
                        lbl.set_markup(&format!("<i>agc {v}</i>"));
                        let _ = ws.send(ClientMsg::CatCommand(
                            cat_commands::set_agc_threshold(v),
                        ));
                    }
                });
            });
        }

        // freq tile → edit frequency (Hz), send CAT, update label + display bar.
        {
            let ws = ws_tx.clone();
            let sp = sdr_params.clone();
            let lc = last_cmd.clone();
            let db = display_bar.clone();
            let lbl = freq_tile_label.clone();
            freq_tile_btn.connect_clicked(move |btn| {
                let current = sp.borrow().freq_hz;
                let ws = ws.clone();
                let sp = sp.clone();
                let lc = lc.clone();
                let db = db.clone();
                let lbl = lbl.clone();
                open_tile_entry(btn, "Frequency (Hz)", &current.to_string(), move |txt| {
                    if let Ok(hz) = txt.replace(['.', ',', ' '], "").parse::<u64>() {
                        lc.set(Instant::now());
                        sp.borrow_mut().freq_hz = hz;
                        db.set_freq_immediate(hz);
                        lbl.set_markup(&format!(
                            "<i>f</i> {}<sup>Hz</sup>",
                            format_freq_spaced(hz)
                        ));
                        let _ = ws.send(ClientMsg::CatCommand(cat_commands::set_freq(hz)));
                    }
                });
            });
        }

        // bw / rit / IF tiles → local label edit only (no CAT command yet).
        {
            let lbl = bw_tile_label.clone();
            bw_tile_btn.connect_clicked(move |btn| {
                let lbl = lbl.clone();
                open_tile_entry(btn, "Filter bandwidth (Hz)", "", move |txt| {
                    if let Ok(v) = txt.trim().parse::<u32>() {
                        lbl.set_markup(&format!("<i>bw</i> {v}<sup>Hz</sup>"));
                    }
                });
            });
        }
        {
            let lbl = rit_tile_label.clone();
            rit_tile_btn.connect_clicked(move |btn| {
                let lbl = lbl.clone();
                open_tile_entry(btn, "RIT offset (Hz)", "", move |txt| {
                    if let Ok(v) = txt.trim().parse::<i32>() {
                        lbl.set_markup(&format!("<i>rit</i> {v:+}<sup>Hz</sup>"));
                    }
                });
            });
        }
        {
            let lbl = if_tile_label.clone();
            if_tile_btn.connect_clicked(move |btn| {
                let lbl = lbl.clone();
                open_tile_entry(btn, "IF offset (Hz)", "", move |txt| {
                    if let Ok(v) = txt.trim().parse::<i32>() {
                        lbl.set_markup(&format!("<i>IF</i> {v:+}<sup>Hz</sup>"));
                    }
                });
            });
        }

        // --- AUDdev / IQdev: class-filtered device pickers ---
        // Each button opens a modal listing only devices of its class.
        // Clicking a row sends `SelectSource(class)` + `SelectDevice`
        // so the server both flips live-pipeline routing and records
        // the pick for the post-respawn state.
        let cached_audio_devices: Rc<RefCell<Vec<DeviceId>>> = Rc::new(RefCell::new(Vec::new()));
        let cached_iq_devices: Rc<RefCell<Vec<DeviceId>>> = Rc::new(RefCell::new(Vec::new()));

        let aud_dev_btn = Button::new();
        {
            let lbl = Label::new(None);
            lbl.set_use_markup(true);
            lbl.set_markup("AUD<sup>dev</sup>");
            aud_dev_btn.set_child(Some(&lbl));
        }
        aud_dev_btn.set_valign(Align::Center);
        aud_dev_btn.set_tooltip_text(Some(
            "Pick an audio input device (FDM-DUO USB audio / HAT / USB dongle)",
        ));
        aud_dev_btn.add_css_class("chrome-btn");
        {
            let ws = ws_tx.clone();
            let cache = cached_audio_devices.clone();
            aud_dev_btn.connect_clicked(move |btn| {
                open_device_picker_anchor(
                    btn.upcast_ref(),
                    "Pick AUD device",
                    SourceClass::Audio,
                    &cache.borrow(),
                    ws.clone(),
                );
            });
        }

        let iq_dev_btn = Button::new();
        {
            let lbl = Label::new(None);
            lbl.set_use_markup(true);
            lbl.set_markup("IQ<sup>dev</sup>");
            iq_dev_btn.set_child(Some(&lbl));
        }
        iq_dev_btn.set_valign(Align::Center);
        iq_dev_btn.set_tooltip_text(Some(
            "Pick an IQ source device (FDM-DUO / HackRF / RSPdx / RTL-SDR)",
        ));
        iq_dev_btn.add_css_class("chrome-btn");
        {
            let ws = ws_tx.clone();
            let cache = cached_iq_devices.clone();
            iq_dev_btn.connect_clicked(move |btn| {
                open_device_picker_anchor(
                    btn.upcast_ref(),
                    "Pick IQ device",
                    SourceClass::Iq,
                    &cache.borrow(),
                    ws.clone(),
                );
            });
        }

        ctrl0_left.append(&aud_dev_btn);
        ctrl0_left.append(&iq_dev_btn);

        // No unconditional `SelectSource` on init — server seeds its own default.

        // --- Overflow row: widgets that aren't in the drawio ---
        let ptt_btn = ToggleButton::with_label("PTT");
        ptt_btn.set_valign(Align::Center);
        {
            let tx = ws_tx.clone();
            ptt_btn.connect_toggled(move |btn| {
                let on = btn.is_active();
                let _ = tx.send(ClientMsg::Ptt(Ptt { on }));
            });
        }
        overflow_row.append(&ptt_btn);

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
            overflow_row.append(&mute_btn);

            let vol_label = Label::new(Some("Vol:"));
            vol_label.add_css_class("monospace");
            overflow_row.append(&vol_label);

            let vol_adj = Adjustment::new(70.0, 0.0, 100.0, 5.0, 10.0, 0.0);
            let vol_scale = Scale::new(Orientation::Horizontal, Some(&vol_adj));
            vol_scale.set_width_request(100);
            vol_scale.set_valign(Align::Center);
            vol_scale.set_draw_value(false);
            let ap = player.clone();
            vol_adj.connect_value_changed(move |adj| {
                ap.set_volume(adj.value() as f32 / 100.0);
            });
            overflow_row.append(&vol_scale);
        }

        overflow_row.append(&flip_btn);
        overflow_row.append(&mode_dropdown);

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
        let rec_btn = ToggleButton::new();
        let rec_label = Label::new(None);
        rec_label.set_markup("<i>REC </i><span foreground=\"red\">\u{25CF}</span>");
        rec_btn.set_child(Some(&rec_label));
        rec_btn.set_valign(Align::Center);
        rec_btn.set_width_request(CTRL_RIGHT_BTN_WIDTH);
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
        // Click spawns `wsjtx` as a detached child. WSJT-X keeps its
        // own config (audio devices, rigctld endpoint) so we don't
        // pass any arguments — the user configures it once on first
        // launch, pointing it at an SSH-tunnelled
        // `localhost:4532` (the rigctld pattern documented in the
        // README). Errors (binary missing, fork failure) are surfaced
        // via stderr; a failed launch doesn't block the button.
        let wsjtx_btn = Button::new();
        let wsjtx_label = Label::new(None);
        wsjtx_label.set_markup("<i>WSJT-X</i>");
        wsjtx_btn.set_child(Some(&wsjtx_label));
        wsjtx_btn.set_valign(Align::Center);
        wsjtx_btn.set_width_request(CTRL_RIGHT_BTN_WIDTH);
        wsjtx_btn.add_css_class("chrome-btn");
        wsjtx_btn.set_tooltip_text(Some(
            "Launch WSJT-X. Point WSJT-X at localhost:4532 once you've \
             set up an SSH tunnel to the Pi's rigctld responder.",
        ));
        wsjtx_btn.connect_clicked(|_| match std::process::Command::new("wsjtx")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(child) => {
                tracing::info!(pid = child.id(), "spawned wsjtx");
                // Intentionally drop the Child handle — WSJT-X is a
                // detached GUI process; we don't wait on it.
            }
            Err(e) => {
                tracing::error!("failed to spawn wsjtx: {e} (is it installed?)");
            }
        });
        ctrl0_right.append(&wsjtx_btn);

        // --- CONFIG dialog (ctrl2-right) ---
        // Phase-5b placeholder: click logs a TODO. The dialog will
        // eventually expose server URL, token, recording dir,
        // start-up DSP defaults.
        let config_btn = Button::new();
        let config_label = Label::new(None);
        config_label.set_markup("<i>CONFIG</i>");
        config_btn.set_child(Some(&config_label));
        config_btn.set_valign(Align::Center);
        config_btn.set_width_request(CTRL_RIGHT_BTN_WIDTH);
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
            ptt_btn,
            aud_dev_btn,
            iq_dev_btn,
            cached_audio_devices,
            cached_iq_devices,
            agc_tile_btn,
            freq_tile_btn,
            freq_tile_label,
            bw_tile_label,
            rit_tile_label,
            if_tile_label,
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

        // Mirror the flags + enabled decoders into the display bar's
        // status line so disp1-center's "NB DNR DNF APF / decode …"
        // row reflects the same state the buttons do.
        self.display_bar.set_dsp_status(
            snap.nb_on,
            snap.dnr_on,
            snap.dnf_on,
            snap.apf_on,
            &snap.enabled_decoders,
        );
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Cache the server's latest `DeviceList`, split by class so the
    /// AUDdev / IQdev pickers each read their own list. Called from
    /// the WS dispatch on every `ServerMsg::DeviceList`. Synthetic
    /// empty-id file placeholders are filtered out.
    pub fn set_device_list(&self, list: &DeviceList) {
        let is_real = |d: &&DeviceId| {
            !(matches!(
                d.kind,
                efd_proto::SourceKind::AudioFile | efd_proto::SourceKind::IqFile,
            ) && d.id.is_empty())
        };
        *self.cached_audio_devices.borrow_mut() =
            list.audio_devices.iter().filter(is_real).cloned().collect();
        *self.cached_iq_devices.borrow_mut() =
            list.iq_devices.iter().filter(is_real).cloned().collect();
    }

    /// Sync control bar from RadioState — stashes the latest for
    /// AUD↔IQ toggle, and refreshes the ctrl0-center tiles so their
    /// Pango markup tracks whatever the radio just reported.
    pub fn sync_from_radio(&self, state: &RadioState) {
        *self.last_radio.borrow_mut() = Some(state.clone());

        self.freq_tile_label.set_markup(&format!(
            "<i>f</i> {}<sup>Hz</sup>",
            format_freq_spaced(state.freq_hz)
        ));
        let bw_text = match state.filter_bw_hz {
            Some(hz) => format_bw_hz(hz),
            None if !state.filter_bw.is_empty() => state.filter_bw.clone(),
            _ => "—".to_string(),
        };
        self.bw_tile_label
            .set_markup(&format!("<i>bw</i> {bw_text}"));
        let rit = if state.rit_on { state.rit_hz } else { 0 };
        self.rit_tile_label
            .set_markup(&format!("<i>rit</i> {rit:+}<sup>Hz</sup>"));
        self.if_tile_label.set_markup(&format!(
            "<i>IF</i> {:+}<sup>Hz</sup>",
            state.if_offset_hz
        ));
    }

    /// Gate UI controls by server-advertised source capabilities.
    pub fn apply_capabilities(&self, caps: &Capabilities) {
        // `control_target == None` means the active source has no CAT
        // surface and no software demod behind it (portable radio, USB
        // dongle). Grey every interactive CAT-plane control. Spectrum,
        // waterfall, display labels, and the device pickers stay live
        // so the user can switch sources.
        let cat_live = caps.control_target != ControlTarget::None;

        self.ptt_btn.set_visible(caps.has_tx);
        self.ptt_btn.set_sensitive(cat_live);
        // AGC + frequency tiles only make sense with hardware CAT.
        // bw/rit/IF tiles remain visible (read-only today) so the drawio
        // layout is stable regardless of CAT availability.
        self.agc_tile_btn.set_visible(caps.has_hardware_cat);
        self.agc_tile_btn.set_sensitive(cat_live);
        self.freq_tile_btn.set_visible(caps.has_hardware_cat);
        self.freq_tile_btn.set_sensitive(cat_live);
        // AUD / IQ availability indicators in the display bar.
        self.display_bar
            .set_source_availability(caps.has_usb_audio, caps.has_iq);
        // Device chips + active-source pill for disp1-left / disp2-left
        // come from Capabilities. `set_passthrough` follows the audio
        // routing — SWDEMOD when the IQ chain produces audio,
        // PASSTHROUGH when the radio's USB audio goes straight through.
        self.display_bar.set_active_device(caps.source);
        let class_tag = if caps.has_iq && !caps.has_usb_audio {
            "IQ"
        } else if caps.has_usb_audio && !caps.has_iq {
            "AUD"
        } else {
            "IQ"
        };
        self.display_bar
            .set_active_source_label(&format!("{:?} {}", caps.source, class_tag));
        self.display_bar.set_passthrough(
            if caps.has_usb_audio && !caps.has_iq { "PASSTHROUGH" } else { "SWDEMOD" },
        );

        // DRM flip toggle — sync initial state from the server's
        // advertised value (usually seeded from its config.toml).
        // Suppressed so the programmatic set_active doesn't round-trip
        // back to the server as a "client wants to change this" message.
        if self.flip_btn.is_active() != caps.drm_flip_spectrum {
            self.suppress_flip_notify.set(true);
            self.flip_btn.set_active(caps.drm_flip_spectrum);
            self.suppress_flip_notify.set(false);
        }

        // AUDdev / IQdev pickers — each visible only when its class
        // actually has devices to pick. These stay sensitive regardless
        // of control_target so the user can switch sources.
        self.aud_dev_btn.set_visible(caps.has_usb_audio);
        self.iq_dev_btn.set_visible(caps.has_iq);

        // Mode selection is greyed whenever there's no CAT target —
        // `control_target` is the single source of truth (covers both
        // the "AUD + portable radio" case and the file-test path).
        self.mode_dropdown.set_sensitive(cat_live);
        for (_, btn) in &self.mode_btns {
            btn.set_sensitive(cat_live);
        }

        // Initial AGC-threshold sync, deferred from construction so we only
        // emit it to sources that can accept the CAT command.
        if caps.has_hardware_cat && caps.control_target == ControlTarget::Radio {
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

    /// Snapshot the last-known SDR params to disk on app quit.
    pub fn save_on_quit(&self) {
        let mut params = self.sdr_params.borrow_mut();
        if let Some(ref state) = *self.last_radio.borrow() {
            params.freq_hz = state.freq_hz;
            params.set_mode(state.mode);
        }
        sdr_params::save(&params);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a ctrl0-center yellow chip tile: a `Button` wrapping a
/// Pango-markup `Label`. Returns both so the caller can wire click
/// handlers on the button and update the markup on the label.
fn make_ctrl_tile(initial_markup: &str, tooltip: &str, width: i32) -> (Button, Label) {
    let btn = Button::new();
    let label = Label::new(None);
    label.set_use_markup(true);
    label.set_markup(initial_markup);
    btn.set_child(Some(&label));
    btn.set_valign(Align::Center);
    btn.set_width_request(width);
    btn.add_css_class("chrome-btn");
    btn.set_tooltip_text(Some(tooltip));
    (btn, label)
}

/// Open a modal single-line-entry dialog anchored on `anchor`. The
/// callback fires on OK or Enter; Cancel closes silently.
fn open_tile_entry<F>(anchor: &Button, title: &str, initial: &str, on_commit: F)
where
    F: Fn(&str) + 'static,
{
    let dlg = Window::new();
    dlg.set_title(Some(title));
    if let Some(root) = anchor.root().and_downcast::<Window>() {
        dlg.set_transient_for(Some(&root));
    }
    dlg.set_modal(true);
    dlg.set_default_size(280, -1);
    dlg.set_resizable(false);

    let vbox = GtkBox::new(Orientation::Vertical, 8);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);

    let entry = Entry::new();
    entry.set_text(initial);
    entry.set_hexpand(true);
    vbox.append(&entry);

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_halign(Align::End);
    let cancel = Button::with_label("Cancel");
    let ok = Button::with_label("OK");
    hbox.append(&cancel);
    hbox.append(&ok);
    vbox.append(&hbox);

    dlg.set_child(Some(&vbox));

    let cb = Rc::new(on_commit);
    {
        let d = dlg.clone();
        cancel.connect_clicked(move |_| d.close());
    }
    {
        let d = dlg.clone();
        let e = entry.clone();
        let cb = cb.clone();
        ok.connect_clicked(move |_| {
            cb(&e.text());
            d.close();
        });
    }
    {
        let d = dlg.clone();
        let cb = cb.clone();
        entry.connect_activate(move |e| {
            cb(&e.text());
            d.close();
        });
    }

    dlg.present();
}

/// Picker dialog — parented to any `Widget`. `class` is the
/// source-class intent of the picker (AUD / IQ); the row-click
/// sends `SelectSource(class)` explicitly rather than deriving it
/// from `DeviceId.kind.class()`. This matters because FDM-DUO's
/// kind is `FdmDuo` in both lists, but its class depends on which
/// list it came from (AUD list → Audio, IQ list → Iq).
fn open_device_picker_anchor(
    anchor: &Widget,
    title: &str,
    class: SourceClass,
    devices: &[DeviceId],
    ws_tx: mpsc::UnboundedSender<ClientMsg>,
) {
    let dlg = Window::new();
    dlg.set_title(Some(title));
    if let Some(root) = anchor.root().and_downcast::<Window>() {
        dlg.set_transient_for(Some(&root));
    }
    dlg.set_modal(true);
    dlg.set_default_size(360, -1);

    let vbox = GtkBox::new(Orientation::Vertical, 6);
    vbox.set_margin_start(12);
    vbox.set_margin_end(12);
    vbox.set_margin_top(12);
    vbox.set_margin_bottom(12);

    if devices.is_empty() {
        let empty = Label::new(Some("No devices discovered yet."));
        empty.set_halign(Align::Start);
        vbox.append(&empty);
    } else {
        for (i, dev) in devices.iter().enumerate() {
            // Skip synthetic file placeholders — those are for the future
            // AudioFile / IqFile replay path and carry no useful `id` today.
            if matches!(
                dev.kind,
                efd_proto::SourceKind::AudioFile | efd_proto::SourceKind::IqFile,
            ) && dev.id.is_empty()
            {
                continue;
            }
            let short = class_short_label(dev, i);
            let btn = Button::with_label(&short);
            btn.set_halign(Align::Fill);
            btn.set_hexpand(true);
            btn.set_tooltip_text(Some(&format!("{:?} — {}", dev.kind, dev.id)));
            let ws = ws_tx.clone();
            let d_clone = dev.clone();
            let dlg_clone = dlg.clone();
            btn.connect_clicked(move |_| {
                // Flip live routing for the current session, then
                // record the pick (which triggers a server respawn
                // so the new pipeline opens the right backend).
                // `class` is the picker's explicit intent (not
                // d.kind.class()) so AUD-FDM doesn't get mistaken
                // for IQ-FDM.
                let _ = ws.send(ClientMsg::SelectSource(class));
                let _ = ws.send(ClientMsg::SelectDevice(d_clone.clone()));
                dlg_clone.close();
            });
            vbox.append(&btn);
        }
    }

    let hbox = GtkBox::new(Orientation::Horizontal, 8);
    hbox.set_halign(Align::End);
    hbox.set_margin_top(6);
    let close = Button::with_label("Close");
    {
        let d = dlg.clone();
        close.connect_clicked(move |_| d.close());
    }
    hbox.append(&close);
    vbox.append(&hbox);

    dlg.set_child(Some(&vbox));
    dlg.present();
}

/// Visual state for a source / device chip in the display bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChipState {
    /// Currently-selected — bright blue `.chip-active`.
    Active,
    /// Available but not selected — dim blue `.chip-inactive`.
    Inactive,
    /// Present in config but not usable on this backend (HackRF
    /// button when the driver isn't wired up) — grey
    /// `.chip-disabled`.
    Disabled,
}

fn make_chip(label: &str) -> Label {
    let l = Label::new(Some(label));
    l.add_css_class("monospace");
    l
}

/// Compact label for a device in a class list. IQ kinds use the
/// three-letter short form (FDM / HRF / RSP / RTL); audio kinds use
/// `AUD{index}` where `index` is the device's position in its class
/// list. Keeps disp1-left and the picker rows narrow.
fn class_short_label(dev: &DeviceId, index_in_class: usize) -> String {
    match dev.kind.class() {
        efd_proto::SourceClass::Audio => format!("AUD{}", index_in_class),
        efd_proto::SourceClass::Iq => device_short_label(dev.kind).to_string(),
    }
}

/// Three-letter short label for a `SourceKind`, shown on the
/// `disp1-left` device chips. Matches the drawio's FDM / HRF
/// style; new kinds pick abbreviations that don't collide.
fn device_short_label(kind: efd_proto::SourceKind) -> &'static str {
    use efd_proto::SourceKind as K;
    match kind {
        K::FdmDuo => "FDM",
        K::HackRf => "HRF",
        K::RspDx => "RSP",
        K::RtlSdr => "RTL",
        K::PortableRadio => "POR",
        K::AudioFile => "AF",
        K::IqFile => "IQF",
    }
}

fn paint_chip(label: &Label, state: ChipState) {
    for class in ["chip-active", "chip-inactive", "chip-disabled"] {
        label.remove_css_class(class);
    }
    match state {
        ChipState::Active => label.add_css_class("chip-active"),
        ChipState::Inactive => label.add_css_class("chip-inactive"),
        ChipState::Disabled => label.add_css_class("chip-disabled"),
    }
}

/// Build the initial tuning-line markup shown before the first
/// `RadioState` arrives. Matches `format_tuning_markup`'s shape so
/// the layout doesn't jump when the first update lands.
fn fallback_tuning_markup() -> String {
    "<i>f</i> <span size='x-large'><b>—</b></span> Hz   demod <b>—</b>   \
     <i>bw</i> <b>—</b> Hz   <i>RIT</i> <b>0</b> Hz   <i>IF</i> <b>0</b> Hz"
        .to_string()
}

/// Render a `RadioState` into a Pango-markup string for
/// `tuning_line`. Matches the drawio IQ-NO-DRM `disp0-center`
/// format.
fn format_tuning_markup(state: &RadioState) -> String {
    let freq = format_freq_spaced(state.freq_hz);
    let mode = mode_markup(state.mode);
    let bw = state
        .filter_bw_hz
        .map(|hz| format_bw_hz(hz))
        .unwrap_or_else(|| state.filter_bw.clone());
    let rit = if state.rit_on { state.rit_hz } else { 0 };
    let if_off = state.if_offset_hz;
    format!(
        "<i>f</i> <span size='x-large'><b>{freq}</b></span> Hz   \
         demod <b>{mode}</b>   \
         <i>bw</i> <b>{bw}</b>   \
         <i>RIT</i> <b>{rit:+}</b> Hz   \
         <i>IF</i> <b>{if_off:+}</b> Hz"
    )
}

/// Mode → Pango-markup string with subscripts for sideband
/// variants. `CW` is CW-upper, `CWR` is CW-lower; narrow FM gets
/// the drawio's `FMₙ` subscript. `SAMU` / `SAML` follow the same
/// convention as `CW` / `CWR`.
fn mode_markup(mode: Mode) -> &'static str {
    match mode {
        Mode::AM => "AM",
        Mode::SAM => "SAM",
        Mode::SAMU => "SAM<sub>u</sub>",
        Mode::SAML => "SAM<sub>l</sub>",
        Mode::DSB => "DSB",
        Mode::LSB => "LSB",
        Mode::USB => "USB",
        Mode::CW => "CW<sub>u</sub>",
        Mode::CWR => "CW<sub>l</sub>",
        Mode::FM => "FM<sub>n</sub>",
        Mode::DRM => "DRM",
        Mode::Unknown => "—",
    }
}

/// Format a Hz value as grouped digits with spaces, matching the
/// drawio's `14 200 000` style rather than the comma/dot format.
fn format_freq_spaced(hz: u64) -> String {
    let raw = hz.to_string();
    let len = raw.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in raw.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// Format a parsed bandwidth in Hz for the tuning line. Sub-kHz
/// values render as e.g. `500 Hz`; kHz values as `2.4 kHz`.
fn format_bw_hz(hz: f64) -> String {
    if hz >= 1000.0 {
        let k = hz / 1000.0;
        if (k - k.round()).abs() < 0.05 {
            format!("{:.0} kHz", k)
        } else {
            format!("{:.1} kHz", k)
        }
    } else {
        format!("{:.0} Hz", hz)
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
