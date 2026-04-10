use efd_proto::{ClientMsg, Ptt, RadioState};
use gtk4::prelude::*;
use gtk4::{Box as GtkBox, Label, LevelBar, Orientation, ToggleButton};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct Controls {
    container: GtkBox,
    freq_label: Label,
    mode_label: Label,
    vfo_label: Label,
    bw_label: Label,
    smeter: LevelBar,
    smeter_label: Label,
    tx_label: Label,
}

impl Controls {
    pub fn new(ws_tx: mpsc::UnboundedSender<ClientMsg>) -> Self {
        let container = GtkBox::new(Orientation::Horizontal, 8);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.set_margin_top(4);
        container.set_margin_bottom(4);

        let vfo_label = Label::new(Some("VFO A"));
        vfo_label.add_css_class("monospace");
        container.append(&vfo_label);

        let freq_label = Label::new(Some("--- Hz"));
        freq_label.add_css_class("monospace");
        freq_label.set_markup("<span font='18' weight='bold'>--- Hz</span>");
        container.append(&freq_label);

        let mode_label = Label::new(Some("---"));
        mode_label.add_css_class("monospace");
        container.append(&mode_label);

        let bw_label = Label::new(Some("BW: ---"));
        bw_label.add_css_class("monospace");
        container.append(&bw_label);

        // S-meter
        let smeter_box = GtkBox::new(Orientation::Horizontal, 4);
        let smeter_title = Label::new(Some("S:"));
        smeter_box.append(&smeter_title);

        let smeter = LevelBar::new();
        smeter.set_min_value(0.0);
        smeter.set_max_value(30.0); // 0=S0, 15=S9, 30=S9+60
        smeter.set_value(0.0);
        smeter.set_width_request(120);
        smeter_box.append(&smeter);

        let smeter_label = Label::new(Some("S0"));
        smeter_label.add_css_class("monospace");
        smeter_box.append(&smeter_label);
        container.append(&smeter_box);

        // TX indicator
        let tx_label = Label::new(Some("RX"));
        tx_label.add_css_class("monospace");
        container.append(&tx_label);

        // Spacer
        let spacer = GtkBox::new(Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        container.append(&spacer);

        // PTT button
        let ptt_btn = ToggleButton::with_label("PTT");
        let tx = ws_tx;
        ptt_btn.connect_toggled(move |btn| {
            let on = btn.is_active();
            let _ = tx.send(ClientMsg::Ptt(Ptt { on }));
        });
        container.append(&ptt_btn);

        Self {
            container,
            freq_label,
            mode_label,
            vfo_label,
            bw_label,
            smeter,
            smeter_label,
            tx_label,
        }
    }

    pub fn widget(&self) -> &GtkBox {
        &self.container
    }

    pub fn update(&self, state: &RadioState) {
        // Format frequency with thousands separators
        let freq = format_freq(state.freq_hz);
        self.freq_label
            .set_markup(&format!("<span font='18' weight='bold'>{freq}</span>"));

        self.mode_label.set_text(&format!("{:?}", state.mode));
        self.vfo_label.set_text(&format!("VFO {:?}", state.vfo));
        self.bw_label
            .set_text(&format!("BW: {}", state.filter_bw));

        // S-meter: convert dBm to Kenwood scale (0-30)
        let s_reading = db_to_s_reading(state.s_meter_db);
        self.smeter.set_value(s_reading as f64);
        self.smeter_label.set_text(&s_reading_to_string(s_reading));

        if state.tx {
            self.tx_label.set_markup("<span foreground='red' weight='bold'>TX</span>");
        } else {
            self.tx_label.set_text("RX");
        }
    }
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
    // S0=-127, S9=-73, S9+60=-13
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
