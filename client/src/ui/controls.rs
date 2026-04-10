use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use efd_proto::{ClientMsg, Mode, Ptt, RadioState};
use gtk4::prelude::*;
use gtk4::{
    Adjustment, Align, Box as GtkBox, Button, DropDown, Entry, Label, LevelBar, Orientation,
    Scale, StringList, ToggleButton,
};
use tokio::sync::mpsc;

use crate::audio::AudioPlayer;
use crate::cat_commands;

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
        let container = GtkBox::new(Orientation::Horizontal, 12);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);
        container.set_halign(Align::Center);

        let vfo_label = Label::new(Some("VFO A"));
        vfo_label.add_css_class("monospace");
        vfo_label.set_width_chars(5);
        vfo_label.set_xalign(0.0);
        container.append(&vfo_label);

        let freq_label = Label::new(Some("--- Hz"));
        freq_label.add_css_class("monospace");
        freq_label.set_width_chars(16);
        freq_label.set_xalign(1.0);
        freq_label.set_markup("<span font='18' weight='bold'>--- Hz</span>");
        container.append(&freq_label);

        let mode_label = Label::new(Some("---"));
        mode_label.add_css_class("monospace");
        mode_label.set_width_chars(5);
        mode_label.set_xalign(0.0);
        container.append(&mode_label);

        let bw_label = Label::new(Some("BW: ---"));
        bw_label.add_css_class("monospace");
        bw_label.set_width_chars(10);
        bw_label.set_xalign(0.0);
        container.append(&bw_label);

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
        container.append(&smeter_box);

        let tx_label = Label::new(Some("RX"));
        tx_label.add_css_class("monospace");
        tx_label.add_css_class("tx-rx-rx");
        tx_label.set_width_chars(2);
        tx_label.set_xalign(0.5);
        container.append(&tx_label);

        Self {
            container,
            freq_label,
            mode_label,
            vfo_label,
            bw_label,
            smeter,
            smeter_label,
            tx_label,
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
    sdr_box: GtkBox,
    freq_entry: Entry,
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

        // --- Mode toggle ---
        let sdr_box = GtkBox::new(Orientation::Horizontal, 8);
        sdr_box.set_visible(false);

        let mode_btn = ToggleButton::with_label("MON");
        mode_btn.set_valign(Align::Center);
        let sb = sdr_box.clone();
        mode_btn.connect_toggled(move |btn| {
            let is_sdr = btn.is_active();
            btn.set_label(if is_sdr { "SDR" } else { "MON" });
            sb.set_visible(is_sdr);
        });
        container.append(&mode_btn);

        // --- SDR controls: frequency only ---
        // Mode/BW/filter are software DSP parameters, not radio commands.

        // Frequency entry
        let freq_entry = Entry::new();
        freq_entry.set_width_chars(14);
        freq_entry.set_placeholder_text(Some("Freq Hz"));
        freq_entry.add_css_class("monospace");
        let tx = ws_tx.clone();
        let lc = last_cmd.clone();
        let db = display_bar.clone();
        freq_entry.connect_activate(move |entry| {
            let text = entry.text();
            if let Ok(hz) = text.replace(['.', ',', ' '], "").parse::<u64>() {
                lc.set(Instant::now());
                db.set_freq_immediate(hz);
                let _ = tx.send(ClientMsg::CatCommand(cat_commands::set_freq(hz)));
            }
        });
        sdr_box.append(&freq_entry);

        // Mode dropdown — controls software demod, not the radio
        let mode_list = StringList::new(&MODES.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let mode_dropdown = DropDown::new(Some(mode_list), gtk4::Expression::NONE);
        mode_dropdown.set_selected(1); // default USB
        mode_dropdown.set_valign(Align::Center);
        let tx = ws_tx.clone();
        mode_dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            if let Some(&(_, mode)) = MODES.get(idx) {
                let _ = tx.send(ClientMsg::SetDemodMode(mode));
            }
        });
        sdr_box.append(&mode_dropdown);

        // Step size dropdown
        let step_list = StringList::new(&STEPS.iter().map(|(s, _)| *s).collect::<Vec<_>>());
        let step_dropdown = DropDown::new(Some(step_list), gtk4::Expression::NONE);
        step_dropdown.set_selected(1); // default 1 kHz
        step_dropdown.set_valign(Align::Center);
        sdr_box.append(&step_dropdown);

        // Tune up/down
        let tune_down = Button::with_label("\u{25BC}"); // ▼
        tune_down.set_valign(Align::Center);
        let fe = freq_entry.clone();
        let sd = step_dropdown.clone();
        let tx = ws_tx.clone();
        let lc = last_cmd.clone();
        let db = display_bar.clone();
        tune_down.connect_clicked(move |_| {
            tune_by_step(&fe, &sd, &tx, &db, &lc, false);
        });
        sdr_box.append(&tune_down);

        let tune_up = Button::with_label("\u{25B2}"); // ▲
        tune_up.set_valign(Align::Center);
        let fe = freq_entry.clone();
        let sd = step_dropdown.clone();
        let tx = ws_tx.clone();
        let lc = last_cmd.clone();
        let db = display_bar.clone();
        tune_up.connect_clicked(move |_| {
            tune_by_step(&fe, &sd, &tx, &db, &lc, true);
        });
        sdr_box.append(&tune_up);

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
            sdr_box,
            freq_entry,
            last_cmd,
        }
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    /// Sync frequency entry from RadioState.
    pub fn sync_from_radio(&self, state: &RadioState) {
        if !self.sdr_box.is_visible() {
            return;
        }
        if self.last_cmd.get().elapsed().as_millis() < 300 {
            return;
        }
        if self.freq_entry.has_focus() {
            return;
        }
        self.freq_entry.set_text(&format!("{}", state.freq_hz));
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
