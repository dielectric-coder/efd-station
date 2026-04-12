use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use efd_proto::{AudioSource, Capabilities, ClientMsg, DrmStatus, Mode, Ptt, RadioState};
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
    /// First DRM info line — mode/bandwidth/modulation/services.
    drm_line1: Label,
    /// Second DRM info line — SNR/WMER/lock flags.
    drm_line2: Label,
    prev: RefCell<Option<CachedState>>,
}

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
        // Outer vertical container: the existing status row on top, plus
        // two optional DRM info lines underneath.
        let container = GtkBox::new(Orientation::Vertical, 2);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);
        container.set_halign(Align::Center);

        let status_row = GtkBox::new(Orientation::Horizontal, 12);
        status_row.set_halign(Align::Center);
        container.append(&status_row);

        let vfo_label = Label::new(Some("VFO A"));
        vfo_label.add_css_class("monospace");
        vfo_label.set_width_chars(5);
        vfo_label.set_xalign(0.0);
        status_row.append(&vfo_label);

        let freq_label = Label::new(Some("--- Hz"));
        freq_label.add_css_class("monospace");
        freq_label.set_width_chars(16);
        freq_label.set_xalign(1.0);
        freq_label.set_markup("<span font='18' weight='bold'>--- Hz</span>");
        status_row.append(&freq_label);

        let mode_label = Label::new(Some("---"));
        mode_label.add_css_class("monospace");
        mode_label.set_width_chars(5);
        mode_label.set_xalign(0.0);
        status_row.append(&mode_label);

        let bw_label = Label::new(Some("BW: ---"));
        bw_label.add_css_class("monospace");
        bw_label.set_width_chars(10);
        bw_label.set_xalign(0.0);
        status_row.append(&bw_label);

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
        smeter_label.set_xalign(0.0);
        smeter_box.append(&smeter_label);
        status_row.append(&smeter_box);

        let tx_label = Label::new(Some("RX"));
        tx_label.add_css_class("monospace");
        tx_label.add_css_class("tx-rx-rx");
        tx_label.set_width_chars(2);
        tx_label.set_xalign(0.5);
        status_row.append(&tx_label);

        // Two DRM info lines. Hidden until the server publishes DrmStatus
        // and re-hidden after a few seconds of staleness (see `update_drm`
        // / `expire_drm_if_stale`).
        let drm_line1 = Label::new(None);
        drm_line1.add_css_class("monospace");
        drm_line1.set_xalign(0.5);
        drm_line1.set_visible(false);
        container.append(&drm_line1);

        let drm_line2 = Label::new(None);
        drm_line2.add_css_class("monospace");
        drm_line2.set_xalign(0.5);
        drm_line2.set_visible(false);
        container.append(&drm_line2);

        Self {
            container,
            freq_label,
            mode_label,
            vfo_label,
            bw_label,
            smeter,
            smeter_label,
            tx_label,
            drm_line1,
            drm_line2,
            prev: RefCell::new(None),
        }
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
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

    /// Update the two DRM info lines with the latest decoder status.
    /// Does NOT affect visibility — pair with `set_drm_visible`.
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

    /// Show or hide the two DRM info lines. Driven by the client's
    /// mode selection (DRM ↔ not-DRM), not by DrmStatus arrival — the
    /// lines stay on while in DRM mode even between status frames, and
    /// carry "---" placeholders until the decoder locks.
    pub fn set_drm_visible(&self, visible: bool) {
        self.drm_line1.set_visible(visible);
        self.drm_line2.set_visible(visible);
        if !visible {
            // Reset text so stale info doesn't flash on re-show.
            self.drm_line1.set_text("");
            self.drm_line2.set_text("");
        } else if self.drm_line1.text().is_empty() {
            // First entry to DRM before any status — show placeholders.
            self.drm_line1
                .set_text("DRM Mode --- · --- · --- · Audio:0 Data:0");
            self.drm_line2
                .set_text("SNR --- · WMER --- · FAC ✗ · SDC ✗ · MSC ✗");
        }
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
    mode_btn: ToggleButton,
    audio_btn: ToggleButton,
    ptt_btn: ToggleButton,
    agc_label: Label,
    agc_scale: Scale,
    mode_dropdown: DropDown,
    mode_list: StringList,
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
    last_cmd: Rc<Cell<Instant>>,
}

impl ControlBar {
    pub fn new(
        ws_tx: mpsc::UnboundedSender<ClientMsg>,
        audio: Option<Arc<AudioPlayer>>,
        display_bar: DisplayBar,
    ) -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 12);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);
        container.set_halign(Align::Center);

        let last_cmd = Rc::new(Cell::new(Instant::now() - std::time::Duration::from_secs(10)));
        let last_radio: Rc<RefCell<Option<RadioState>>> = Rc::new(RefCell::new(None));
        let sdr_params = Rc::new(RefCell::new(sdr_params::load()));

        // --- SDR controls box (visible in SDR mode only) ---
        let sdr_box = GtkBox::new(Orientation::Horizontal, 8);
        sdr_box.set_visible(false);

        // Create SDR widgets first so mode toggle handler can reference them.
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

        // Dropdown is populated from `active_modes`, which defaults to all MODES
        // and is re-filtered when server `Capabilities` arrive.
        let active_modes: Rc<RefCell<Vec<(&'static str, Mode)>>> =
            Rc::new(RefCell::new(MODES.to_vec()));
        let suppress_mode_notify = Rc::new(Cell::new(false));
        let mode_list = StringList::new(&MODES.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let mode_dropdown = DropDown::new(Some(mode_list.clone()), gtk4::Expression::NONE);
        mode_dropdown.set_selected(1); // default USB
        mode_dropdown.set_valign(Align::Center);
        {
            let tx = ws_tx.clone();
            let sp = sdr_params.clone();
            let am = active_modes.clone();
            let suppress = suppress_mode_notify.clone();
            let db = display_bar.clone();
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
                    // Reveal/hide the two DRM display lines based on user
                    // mode intent, independent of whether the bridge has
                    // produced a frame yet. Placeholder "---" values are
                    // fine until the first real status arrives.
                    db.set_drm_visible(mode == Mode::DRM);
                }
            });
        }
        sdr_box.append(&mode_dropdown);

        // --- Audio source toggle (MON mode only) ---
        let audio_btn = ToggleButton::with_label("USB");
        audio_btn.set_valign(Align::Center);
        audio_btn.set_tooltip_text(Some("USB = radio hardware demod, SW = software demod"));
        {
            let tx = ws_tx.clone();
            audio_btn.connect_toggled(move |btn| {
                if btn.is_active() {
                    // MON+USB → MON+SW: demod mirrors radio params
                    btn.set_label("SW");
                    let _ = tx.send(ClientMsg::SetDemodMode(None));
                    let _ = tx.send(ClientMsg::SetAudioSource(AudioSource::SoftwareDemod));
                } else {
                    // MON+SW → MON+USB
                    btn.set_label("USB");
                    let _ = tx.send(ClientMsg::SetAudioSource(AudioSource::RadioUsb));
                }
            });
        }

        // --- MON/SDR mode toggle ---
        let mode_btn = ToggleButton::with_label("MON");
        mode_btn.set_valign(Align::Center);
        {
            let sb = sdr_box.clone();
            let ab = audio_btn.clone();
            let tx = ws_tx.clone();
            let lr = last_radio.clone();
            let sp = sdr_params.clone();
            let fe = freq_entry.clone();
            let md = mode_dropdown.clone();
            let am = active_modes.clone();
            let db = display_bar.clone();
            mode_btn.connect_toggled(move |btn| {
                let is_sdr = btn.is_active();
                btn.set_label(if is_sdr { "SDR" } else { "MON" });
                sb.set_visible(is_sdr);
                ab.set_visible(!is_sdr);

                if is_sdr {
                    // --- MON → SDR ---
                    let (freq_hz, mode) = {
                        let params = sp.borrow();
                        (params.freq_hz, params.mode())
                    };
                    let _ = tx.send(ClientMsg::CatCommand(cat_commands::set_freq(freq_hz)));
                    if let Some(cmd) = cat_commands::set_mode(mode) {
                        let _ = tx.send(ClientMsg::CatCommand(cmd));
                    }
                    let _ = tx.send(ClientMsg::SetDemodMode(Some(mode)));
                    let _ = tx.send(ClientMsg::SetAudioSource(AudioSource::SoftwareDemod));

                    // Update SDR UI controls (set_selected fires notify handler that borrows sp)
                    fe.set_text(&format!("{}", freq_hz));
                    if let Some(idx) = am.borrow().iter().position(|&(_, m)| m == mode) {
                        md.set_selected(idx as u32);
                    }
                    db.set_drm_visible(mode == Mode::DRM);
                } else {
                    // --- SDR → MON ---
                    db.set_drm_visible(false);
                    {
                        let mut params = sp.borrow_mut();
                        if let Some(ref state) = *lr.borrow() {
                            params.freq_hz = state.freq_hz;
                            params.set_mode(state.mode);
                        }
                        sdr_params::save(&params);
                    }

                    let _ = tx.send(ClientMsg::SetDemodMode(None));

                    if ab.is_active() {
                        let _ = tx.send(ClientMsg::SetAudioSource(AudioSource::SoftwareDemod));
                    } else {
                        let _ = tx.send(ClientMsg::SetAudioSource(AudioSource::RadioUsb));
                    }
                }
            });
        }

        container.append(&mode_btn);
        container.append(&audio_btn);

        // --- AGC threshold slider (always visible, 0–10) ---
        let initial_threshold = sdr_params.borrow().agc_threshold;
        let agc_label = Label::new(Some("AGC:"));
        agc_label.add_css_class("monospace");
        agc_label.set_valign(Align::Center);
        container.append(&agc_label);

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
        container.append(&agc_scale);

        // Audio-source initial sync is always safe; the AGC-threshold initial
        // sync is deferred to `apply_capabilities` so it's gated on
        // has_hardware_cat and not emitted to sources that can't accept it.
        let _ = ws_tx.send(ClientMsg::SetAudioSource(AudioSource::RadioUsb));

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

        container.append(&sdr_box);

        // --- Always-visible controls: PTT, Mute, Volume ---
        let ptt_btn = ToggleButton::with_label("PTT");
        ptt_btn.set_valign(Align::Center);
        let tx = ws_tx.clone();
        ptt_btn.connect_toggled(move |btn| {
            let on = btn.is_active();
            let _ = tx.send(ClientMsg::Ptt(Ptt { on }));
        });
        container.append(&ptt_btn);

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
            container.append(&mute_btn);

            let vol_label = Label::new(Some("Vol:"));
            vol_label.add_css_class("monospace");
            container.append(&vol_label);

            let vol_adj = Adjustment::new(70.0, 0.0, 100.0, 5.0, 10.0, 0.0);
            let vol_scale = Scale::new(Orientation::Horizontal, Some(&vol_adj));
            vol_scale.set_width_request(100);
            vol_scale.set_valign(Align::Center);
            vol_scale.set_draw_value(false);
            let ap = player.clone();
            vol_adj.connect_value_changed(move |adj| {
                ap.set_volume(adj.value() as f32 / 100.0);
            });
            container.append(&vol_scale);
        }

        Self {
            container,
            mode_btn,
            audio_btn,
            ptt_btn,
            agc_label,
            agc_scale,
            mode_dropdown,
            mode_list,
            active_modes,
            suppress_mode_notify,
            ws_tx,
            last_radio,
            sdr_params,
            last_cmd,
        }
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
        // MON/SDR toggle is only meaningful when the source can supply IQ.
        self.mode_btn.set_visible(caps.has_iq);
        // USB audio passthrough and AGC threshold are FDM-DUO CAT features.
        self.audio_btn.set_visible(caps.has_hardware_cat);
        self.agc_label.set_visible(caps.has_hardware_cat);
        self.agc_scale.set_visible(caps.has_hardware_cat);

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

    /// Save SDR params if currently in SDR mode (call on app quit).
    pub fn save_on_quit(&self) {
        if self.mode_btn.is_active() {
            // In SDR mode — save current params
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
