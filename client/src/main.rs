mod audio;
mod cat_commands;
pub mod sdr_params;
mod ws;
mod ui;

use std::sync::{Arc, Mutex};

use efd_proto::{FftBins, RadioState, ServerMsg};
use gtk4::prelude::*;
use gtk4::{gdk, Application, ApplicationWindow, CssProvider};

const APP_ID: &str = "com.dielectriccoder.efd-client";

fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ws://127.0.0.1:8080/ws".to_string());

    let app = Application::builder().application_id(APP_ID).build();

    let url2 = url.clone();
    app.connect_activate(move |app| build_ui(app, &url2));

    app.run_with_args::<String>(&[]);
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_string(
        "
        * {
            font-family: 'Hack Nerd Font Mono', monospace;
            font-size: 11pt;
        }
        .tx-rx-rx {
            background-color: #2e7d32;
            color: white;
            border-radius: 4px;
            padding: 2px 6px;
        }
        .tx-rx-tx {
            background-color: #c62828;
            color: white;
            border-radius: 4px;
            padding: 2px 6px;
        }
        .app-mode {
            background-color: #bbdefb;
            color: #0d47a1;
            font-size: 13pt;
            font-weight: bold;
            border-radius: 4px;
            padding: 2px 8px;
        }
        .app-mode-warn {
            background-color: #fff176;
            color: #5d4037;
        }
        .app-mode-disabled {
            background-color: #eeeeee;
            color: #9e9e9e;
            font-size: 13pt;
            font-weight: bold;
            border-radius: 4px;
            padding: 2px 8px;
        }
        .spectrum-controls {
            background-color: rgba(0, 0, 0, 0.5);
            border-radius: 4px;
            padding: 4px 8px;
            border: none;
        }
        .spectrum-controls label {
            color: rgba(255, 255, 255, 0.8);
        }
        .spectrum-controls spinbutton {
            background-color: rgba(0, 0, 0, 0.3);
            color: white;
            border: none;
        }
        /* Phase 5b: drawio IQ-NO-DRM control-bar colour palette. */
        /* DSP / chrome toggles (NB, APF, DNR, DNF, REC, CONFIG,
         * WSJT-X, SRC, DEV) share the yellow chip look. */
        .dsp-toggle, .chrome-btn {
            background: #fff2cc;
            color: #333;
            border: 1px solid #d6b656;
            border-radius: 4px;
            padding: 2px 10px;
            min-width: 36px;
        }
        .dsp-toggle:checked, .chrome-btn:checked {
            background: #ffd54f;
            color: #000;
        }
        /* IF-demod mode buttons — orange, per drawio. */
        .mode-btn {
            background: #f0a30a;
            color: #000;
            border: 1px solid #bd7000;
            border-radius: 4px;
            padding: 2px 10px;
            min-width: 40px;
        }
        .mode-btn:checked {
            background: #ffb300;
            color: #000;
            font-weight: bold;
        }
        /* Audio-domain decoders — purple. */
        .decoder-audio {
            background: #e1d5e7;
            color: #3a214b;
            border: 1px solid #9673a6;
            border-radius: 4px;
            padding: 2px 10px;
            min-width: 40px;
        }
        .decoder-audio:checked {
            background: #b39ddb;
            color: #000;
            font-weight: bold;
        }
        /* DRM / FreeDV decoders — pink. */
        .decoder-drm {
            background: #f8cecc;
            color: #6a1f1f;
            border: 1px solid #b85450;
            border-radius: 4px;
            padding: 2px 10px;
            min-width: 40px;
        }
        .decoder-drm:checked {
            background: #ef9a9a;
            color: #000;
            font-weight: bold;
        }
        /* Display-bar chip palette, matching drawio IQ-NO-DRM. */
        /* Blue source / device pills (AUD / IQ / FDM / HRF). */
        .chip-active {
            background: #1ba1e2;
            color: #fff;
            font-weight: bold;
            border-radius: 10px;
            padding: 1px 10px;
            min-width: 30px;
        }
        .chip-inactive {
            background: #bbdefb;
            color: #0d47a1;
            border-radius: 10px;
            padding: 1px 10px;
            min-width: 30px;
        }
        .chip-disabled {
            background: #eeeeee;
            color: #9e9e9e;
            border-radius: 10px;
            padding: 1px 10px;
            min-width: 30px;
        }
        /* Green active-source pill (e.g. FDM IQ). */
        .chip-source {
            background: #60a917;
            color: #fff;
            font-weight: bold;
            border-radius: 4px;
            padding: 2px 10px;
        }
        /* Gray audio-routing indicator (PASSTHROUGH / SWDEMOD). */
        .chip-passthrough {
            background: #647687;
            color: #fff;
            border-radius: 4px;
            padding: 2px 10px;
            font-weight: bold;
        }
        /* Yellow tuning chip (f / bw / rit / IF readouts). */
        .chip-tuning {
            background: #fff2cc;
            color: #000;
            border: 1px solid #d6b656;
            border-radius: 10px;
            padding: 1px 8px;
        }
        ",
    );
    gtk4::style_context_add_provider_for_display(
        &gdk::Display::default().expect("Could not get default display"),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &Application, url: &str) {
    load_css();

    // Shared state — written by WS thread, read by GTK draw funcs
    let fft_data: Arc<Mutex<Option<FftBins>>> = Arc::new(Mutex::new(None));
    let radio_state: Arc<Mutex<Option<RadioState>>> = Arc::new(Mutex::new(None));

    // Message queue from WS thread to GTK main loop
    let msg_queue: Arc<Mutex<Vec<ServerMsg>>> = Arc::new(Mutex::new(Vec::new()));

    // Audio player
    let audio_player = match audio::AudioPlayer::new() {
        Ok(p) => {
            eprintln!("audio: output stream started");
            Some(Arc::new(p))
        }
        Err(e) => {
            eprintln!("audio: failed to start ({e}), running without audio");
            None
        }
    };

    // Start WS connection
    let ws_tx = ws::start(url, msg_queue.clone());

    // Build window
    let window = ApplicationWindow::builder()
        .application(app)
        .title("efd-station")
        .default_width(1024)
        .default_height(700)
        .build();

    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    main_box.set_margin_start(4);
    main_box.set_margin_end(4);
    main_box.set_margin_top(4);
    main_box.set_margin_bottom(4);

    let display_bar = ui::controls::DisplayBar::new();
    main_box.append(display_bar.widget());

    let (spectrum, display_range) =
        ui::spectrum::Spectrum::new(fft_data.clone(), radio_state.clone());
    spectrum.widget().set_vexpand(true);
    main_box.append(spectrum.widget());

    let waterfall =
        ui::waterfall::Waterfall::new(display_range, fft_data.clone(), radio_state.clone());
    waterfall.widget().set_vexpand(true);
    main_box.append(waterfall.widget());

    let control_bar =
        ui::controls::ControlBar::new(ws_tx.clone(), audio_player.clone(), display_bar.clone());
    main_box.append(control_bar.widget());

    window.set_child(Some(&main_box));

    // Poll message queue from GTK main loop (60 fps tick)
    let fft_data2 = fft_data.clone();
    let radio_state2 = radio_state.clone();
    let spectrum2 = spectrum.clone();
    let waterfall2 = waterfall.clone();
    let display_bar2 = display_bar.clone();
    let control_bar2 = control_bar.clone();
    let audio2 = audio_player.clone();
    let queue = msg_queue.clone();

    glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
        let msgs: Vec<ServerMsg> = {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            q.drain(..).collect()
        };

        let mut need_redraw = false;

        for msg in msgs {
            match msg {
                ServerMsg::FftBins(bins) => {
                    waterfall2.push_line(&bins.bins);
                    *fft_data2.lock().unwrap_or_else(|e| e.into_inner()) = Some(bins);
                    need_redraw = true;
                }
                ServerMsg::RadioState(state) => {
                    display_bar2.update(&state);
                    control_bar2.sync_from_radio(&state);
                    *radio_state2.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
                }
                ServerMsg::Audio(chunk) => {
                    if let Some(ref player) = audio2 {
                        player.push_audio(&chunk.opus_data);
                    }
                }
                ServerMsg::Capabilities(caps) => {
                    eprintln!(
                        "server capabilities: source={:?} iq={} tx={} hw_cat={} usb_audio={} modes={:?}",
                        caps.source,
                        caps.has_iq,
                        caps.has_tx,
                        caps.has_hardware_cat,
                        caps.has_usb_audio,
                        caps.supported_demod_modes,
                    );
                    control_bar2.apply_capabilities(&caps);
                }
                ServerMsg::DrmStatus(status) => {
                    display_bar2.update_drm(&status);
                }
                ServerMsg::Error(err) => {
                    eprintln!("server error: [{}] {}", err.code, err.message);
                }
                ServerMsg::DeviceList(list) => {
                    display_bar2.set_device_list(&list);
                    control_bar2.set_device_list(&list);
                    // When the active source is audio-class (AUD), the
                    // server gates IQ FFT frames and doesn't yet produce
                    // an audio-domain spectrum. Clear the stale IQ
                    // spectrum so the display isn't lying about what's
                    // live. A subsequent IQ pick will repopulate on the
                    // next FftBins frame.
                    if list
                        .active
                        .as_ref()
                        .map(|d| matches!(d.kind.class(), efd_proto::SourceClass::Audio))
                        .unwrap_or(false)
                    {
                        *fft_data2.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        need_redraw = true;
                    }
                }
                ServerMsg::DecodedText(dt) => {
                    display_bar2.push_decoded(dt.decoder, &dt.text);
                }
                ServerMsg::RecordingStatus(rs) => {
                    control_bar2.apply_rec_status(&rs);
                }
                ServerMsg::StateSnapshot(snap) => {
                    control_bar2.apply_snapshot(&snap);
                }
            }
        }

        if need_redraw {
            spectrum2.queue_draw();
            waterfall2.queue_draw();
        }

        glib::ControlFlow::Continue
    });

    // Save SDR params on window close
    let cb = control_bar.clone();
    window.connect_close_request(move |_| {
        cb.save_on_quit();
        glib::Propagation::Proceed
    });

    window.present();
}
